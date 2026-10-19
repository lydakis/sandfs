# Changelog

## 0.4.1

- **README**: rewritten for newcomers. It explains what sandfs is for in plain terms, starts with one short example, then covers each kind of source (with a git example), giving a workspace to an agent, mounting it as a folder, a performance table and the limits.
- **sandfs-cli speed**: the FUSE mount tells the kernel it needn't open or close files, so a first read of a small file is about a quarter faster and rereads and overwrites take about 40% less time.
- **sandfs-cli saving**: closing a file no longer commits it. A write is saved once its file has gone 250 ms without a write, on `fsync`, or at `unmount`, `fork` or `changes`. A program that writes a file, closes it and exits still has it saved a moment later, but not at the instant of the close.
- **sandfs-cli ranged reads**: large host-folder files read in ranges go back to the folder only for the parts the kernel hasn't kept in its page cache.
- **Benchmarks**: `sandfs-bench --remote URL` mounts a large repository from a git server and compares it with full, shallow, blobless and sparse clones: time until the files are readable, scattered reads, a folder search, a second copy, peak memory and disk.

## 0.4.0 (2026-10-19)

A rewrite. sandfs is now a general-purpose sandbox filesystem with a Rust core, and the Python package is a thin binding over it. Nothing from the 0.2 API carries over.

- **Workspaces**: mount read-only sources at paths, keep every write, directory and deletion in a copy-on-write overlay, and fork in O(1). Forks share everything already read but never see each other's changes.
- **Sources**: a host directory (`DirSource`), in-memory files (`MemorySource`), a git commit read without a checkout (`GitSource`, faster in a partial clone), or any Python object with `list(path)` and `read(path)`.
- **Changes**: `changes()` lists every added, modified or deleted file. The harness decides what to keep and writes it back.
- **Bytes first**: files are bytes of any kind. `stat` always reports an exact size and a version, and `write(..., expected_version=...)` rejects a write to a file that changed since it was read.
- **Large files**: `read(path, offset=..., length=...)` reads one range without caching the whole file. A source can define `read_range` to serve ranges itself.
- **Concurrency**: calls release the GIL, and forks reading the same uncached file fetch it from the source once.
- **sandfs-mcp**: an MCP server exposing a copy-on-write workspace as nine file tools (read, list, glob, grep, write, edit, move, delete, changes); `sandfs-mcp --dir MOUNT=PATH`. A path holding a control character or line separator is shown as a quoted JSON string, so one name can't read as two lines.
- **sandfs-cli**: the `sandfs` command mounts a workspace as a real directory over FUSE on Linux; `sandfs mount`, `fork`, `changes`, `unmount`. Sources are a host folder or `git:<path or url>[@rev]`; a remote repository mounts in about a second, since only the mounted commit and its folder listings are fetched, with no history. File contents are fetched when first read. A branch or tag name is looked up on the server at every mount; a commit id mounted before also mounts offline. Changes are saved on disk between mounts. `sandfs serve --mcp NAME` serves a workspace to an agent as MCP file tools instead.
- **sandfs-cli speed**: the FUSE mount keeps the kernel's cache for files that haven't changed, lists directories with their attributes, skips flushes on files opened read-only and polls for mounts in milliseconds, so rereading small files takes half the time, stat after a listing is 25× faster and `sandfs fork` returns in 7 ms instead of 54.
- **sandfs-core speed**: workspaces resolve a path in one lookup instead of one per directory and host-folder reads check the file they opened instead of resolving each parent folder first, so a stat 64 folders deep is about 12× faster and creates and deletes about a third faster.
- **sandfs-cli concurrency**: one process serves a workspace at a time. A second mount or `serve --mcp` of a workspace in use is refused instead of saving over the first, as is a second mount on the same directory. `unmount` fails if the final save failed, `serve --mcp` undoes and reports a change it couldn't save, and `changes` reads a workspace safely while another process is saving it. Building `sandfs-cli` now needs Rust 1.89 (the library crates still build with 1.85).
- **Benchmarks**: `sandfs-bench`, an unpublished crate, times sandfs through the Rust API and the FUSE mount against the host filesystem and overlayfs, including a workflow of parallel attempts that search, edit and check their files (`--only workflow`); `cargo run --release -p sandfs-bench -- --scratch /dev/shm`.
- **sandfs-cli on macOS and Windows**: the `sandfs` command builds there with everything but the mount, and `sandfs serve --mcp SOURCE` creates a workspace over a source. The library crates and the Python package are tested on macOS and Windows in CI.
- **Packaging**: one abi3 wheel per platform covers Python 3.11 and later. The Rust crates `sandfs-core`, `sandfs-git`, `sandfs-mcp` and `sandfs-cli` are published alongside.
- **Removed**: the bash-like shell, host-command bridge, Python executor, access policies, storage adapters, write hooks, symlinks, text edit and unified diffs.

## 0.2.2 and earlier

The original pure-Python design. See the git history.
