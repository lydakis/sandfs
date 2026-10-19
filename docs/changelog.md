# Changelog

## Unreleased

- **sandfs-mcp**: `list`, `glob`, `grep` and `changes` show a path holding a control character or line separator, such as a newline, as a quoted JSON string, so one name can't read as two lines.

## 0.4.0 (2026-10-18)

A rewrite. sandfs is now a general-purpose sandbox filesystem with a Rust core, and the Python package is a thin binding over it. Nothing from the 0.2 API carries over.

- **Workspaces**: mount read-only sources at paths, keep every write, directory and deletion in a copy-on-write overlay, and fork in O(1). Forks share everything already read but never see each other's changes.
- **Sources**: a host directory (`DirSource`), in-memory files (`MemorySource`), a git commit read without a checkout (`GitSource`, faster in a partial clone), or any Python object with `list(path)` and `read(path)`.
- **Changes**: `changes()` lists every added, modified or deleted file. The harness decides what to keep and writes it back.
- **Bytes first**: files are bytes of any kind. `stat` always reports an exact size and a version, and `write(..., expected_version=...)` rejects a write to a file that changed since it was read.
- **Large files**: `read(path, offset=..., length=...)` reads one range without caching the whole file. A source can define `read_range` to serve ranges itself.
- **Concurrency**: calls release the GIL, and forks reading the same uncached file fetch it from the source once.
- **sandfs-mcp**: an MCP server exposing a copy-on-write workspace as nine file tools (read, list, glob, grep, write, edit, move, delete, changes); `sandfs-mcp --dir MOUNT=PATH`.
- **sandfs-cli**: the `sandfs` command mounts a workspace as a real directory over FUSE on Linux; `sandfs mount`, `fork`, `changes`, `unmount`. Sources are a host folder or `git:<path or url>[@rev]`, and changes are saved on disk between mounts. `sandfs serve --mcp NAME` serves a workspace to an agent as MCP file tools instead.
- **sandfs-cli speed**: the FUSE mount keeps the kernel's cache for files that haven't changed, lists directories with their attributes, skips flushes on files opened read-only and polls for mounts in milliseconds, so rereading small files takes half the time, stat after a listing is 25× faster and `sandfs fork` returns in 7 ms instead of 54.
- **sandfs-core speed**: workspaces resolve a path in one lookup instead of one per directory and host-folder reads check the opened file instead of statting the path again, so a stat 64 folders deep is about 12× faster and creates and deletes about a third faster.
- **Benchmarks**: `sandfs-bench`, an unpublished crate, times sandfs through the Rust API and the FUSE mount against the host filesystem and overlayfs; `cargo run --release -p sandfs-bench -- --scratch /dev/shm`.
- **Packaging**: one abi3 wheel per platform covers Python 3.11 and later. The Rust crates `sandfs-core`, `sandfs-git`, `sandfs-mcp` and `sandfs-cli` are published alongside.
- **Removed**: the bash-like shell, host-command bridge, Python executor, access policies, storage adapters, write hooks, symlinks, text edit and unified diffs.

## 0.2.2 and earlier

The original pure-Python design. See the git history.
