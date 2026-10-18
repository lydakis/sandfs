# sandfs Roadmap

sandfs is the filesystem you give an agent instead of a bash tool. The goal is a small, solid primitive: lazy mounts of real sources (a git commit, a host directory), a copy-on-write workspace on top that forks in O(1), and a diff of what the agent changed.

The full design lives in the interface spec linked from the project. This file tracks where the code is.

## Direction

The core is being rewritten in Rust (`crates/`). Python and TypeScript become thin bindings over it, and agents reach it through MCP file tools. Execution stays out of scope: sandfs is where an agent reads and writes, not the sandbox that runs its commands.

Python 0.3 is the last release of the original design. The Rust-backed release keeps only the primitive and drops the rest:

| Kept | Dropped |
| --- | --- |
| Workspaces, mounts, fork, diff, edit | Bash-like shell and its command set |
| Lazy git and directory sources | Host bridge (`host`, `bash`, `sh`, host fallback) |
| MCP file tools for agents | Python executor |
| | Storage adapters, write hooks, full-text index |

Access policies come back in a smaller form only if a real use needs them.

## Build order

1. **Core** (`sandfs-core`). Workspace over read-only sources, overlay, fork, diff, exact-string edits. Done.
2. **Git source** (`sandfs-git`). Commit trees read through `git cat-file --batch`; a `blob:none` partial clone fetches only the blobs that are read. Done.
3. **Python bindings** (PyO3), with `sandfs.Workspace` replacing `VirtualFileSystem`.
4. **MCP server** (`sandfs-mcp`): read, list, glob, grep, write, edit, move, delete and diff over one workspace. Forks, checkpoints and commits stay with the harness.
5. **CLI and FUSE**: `sandfs mount`, `sandfs worktree add`, `sandfs diff`, `sandfs serve --mcp`.
6. **Commit back**: turn a workspace diff into a git commit or a pull request.
7. **TypeScript bindings** (napi-rs, later WASM).

## Known gaps in the core

- Symlinks are followed only as the final path component; a path through a directory symlink is not found.
- Directory renames copy entries by reference, so they cost one overlay entry per file in the subtree.
- Git listings don't carry file sizes, so `stat` on an unread git file has no size.
