# sandfs Roadmap

sandfs is a general-purpose sandbox filesystem for agents. An agent works on files of any kind (documents, spreadsheets, images, data sets, notes, configs, code) in a workspace it can change freely and throw away, and the harness decides which changes to keep. Code is one use case among many.

The core has three primitives: a **source** (a read-only tree read lazily), a **workspace** (sources mounted at paths plus a copy-on-write overlay) and a **change** (one file that differs, from `changes()`). Snapshots and checkpoints are forks, not new concepts. The full design lives in the interface spec linked from the project; this file tracks where the code is.

## Direction

- **Bytes first.** A file is bytes; text tools (search, edit, line-numbered reads) skip files that aren't text.
- **Sources are open.** The core ships a host directory and in-memory files. Anything else (an API, an object store, a database) is a source the harness writes with two calls, `list` and `read`, in Python or Rust. A lazy git source is an optional crate.
- **Changes are files, not patches.** The harness walks `changes()` and writes each changed file back to wherever its data lives.
- **Execution stays out of scope.** Real tools run in a container or on the host against a FUSE mount.

0.2.2 was the last published release of the original design, and 0.4.0 is the first of this one. The original design's shell, host bridge, Python executor, storage adapters, write hooks and index are gone, and so are symlinks and core text edits, which only code needed.

## Build order

1. **Core** (`sandfs-core`): sources, overlay, fork, changes. Done.
2. **Python bindings** (`sandfs-py`, PyO3 and maturin): `sandfs.Workspace`, built-in directory, in-memory and git sources, and sources written in Python. Done.
3. **MCP server**: read, list, glob, grep, write, edit, move, delete and changes over one workspace. Text edit, glob and search live here, built from read and a versioned write. Done (`sandfs-mcp`).
4. **CLI and FUSE** (`sandfs-cli`, mounting on Linux): `sandfs mount`, `sandfs fork`, `sandfs changes`, `sandfs serve --mcp`. Done.
5. **Ranged reads**: `read_range` on sources and workspaces, so a large file is read in pieces instead of whole. Done.
6. **TypeScript bindings**, when a caller needs them.

## Known gaps in the core

- Directory renames copy entries by reference, so they cost one overlay entry per file in the subtree.
- A whole read of a base file is cached for the workspace's lifetime, so reading a multi-gigabyte file whole (not in ranges) keeps it in memory. Writes always replace a whole file.
- A source that leaves out sizes (git does) makes `stat` read the file to answer.

## Known gaps in the CLI

- A mounted workspace holds its writes in memory until they are saved, so it costs memory in proportion to what was written.
- Only the executable bit of a file's mode is kept; owners, other mode bits and extended attributes are not.
- No hard links, symlinks or special files, matching the core.
- macOS and Windows have no mount yet. On macOS, a mount served over NFS on localhost would need nothing installed.
