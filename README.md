# sandfs

`sandfs` is a sandbox filesystem for agents. Mount any data without copying it (a folder, in-memory files, an API, a bucket, a git commit), let the agent read and write through a copy-on-write workspace, fork that workspace in memory in O(1) for every attempt, and collect the files it changed. Files are bytes of any kind: documents, spreadsheets, images, data sets and code alike.

> **Status**: early. The core is Rust (`crates/`), and the Python package is a thin binding over it. APIs may still change.

## Primitives

- **Source**: a read-only tree, read only when a path below it is touched. sandfs ships a host directory, in-memory files and an optional git source; anything else is two methods you write, `list` and `read`.
- **Workspace**: sources mounted at paths, plus a copy-on-write overlay that holds every write, directory and deletion. Sources are never modified.
- **Change**: one file that differs from the mounts, returned by `changes()`.

Everything else is a composition. A snapshot is a fork nobody writes to, a working copy on disk is a fork mounted over FUSE, and saving is the harness walking `changes()` and writing each changed file wherever its data lives.

## Installation

```bash
pip install "sandfs>=0.4"   # prebuilt wheels for Linux, macOS and Windows, Python 3.11+
cargo add sandfs-core       # the Rust core; sandfs-git adds the git source
cargo install sandfs-mcp    # an MCP server with file tools over a workspace
```

Until 0.4.0 is published, install from git instead with `pip install git+https://github.com/lydakis/sandfs.git`, which builds the extension and needs a Rust toolchain.

## Quickstart

```python
import sandfs

class Tickets:
    """Any data can be a source: list a directory, read a file."""

    def list(self, path):
        return [sandfs.DirEntry("T-1.json"), sandfs.DirEntry("T-2.json")]

    def read(self, path):
        return fetch_ticket_json(path.removeprefix("/").removesuffix(".json"))

base = sandfs.Workspace()
base.mount("/reports", sandfs.DirSource("./reports"))   # a host folder, never written to
base.mount("/tickets", Tickets())                        # read only when the agent looks
base.mount("/brief", sandfs.MemorySource({"task.md": "Summarise open tickets\n"}))

attempt = base.fork()                                    # one fork per agent or attempt
version = attempt.write("/reports/summary.md", "# Open tickets\n")
attempt.write("/reports/summary.md", "# Open tickets (2)\n", expected_version=version)
attempt.write("/reports/chart.png", png_bytes)

for change in attempt.changes():
    print(change.kind, change.path)                      # added /reports/chart.png, added /reports/summary.md
```

`base` is unchanged: forks share the mounts and everything already read, but never see each other's changes.

A source must return the same answer every time, because what it returns is cached and shared between forks. Sizes in `DirEntry` are optional; `stat` reads a file once to find a size the source left out.

Large files can be read in pieces: `read(path, offset=..., length=...)` asks the source for just that range and doesn't cache it, so a multi-gigabyte data set or video is never held in memory whole. `DirSource` seeks on disk, and a Python source can define `read_range(path, offset, length)`; without it, the range is sliced from a whole read.

`sandfs.GitSource(repo, rev="HEAD")` mounts a commit of a local repository without a checkout. In a partial clone (`git clone --filter=blob:none`), git fetches a file's contents only when it is first read.

## API

| Call | Does |
| --- | --- |
| `mount(path, source)` | Mount a `DirSource(root)`, `MemorySource({name: str or bytes})`, `GitSource(repo, rev)`, or any object with `list(path)` and `read(path)` |
| `fork()` | A new workspace with the same mounts and changes, in O(1) |
| `read(path, *, offset=0, length=None)`, `read_text(path)` | File contents as bytes (all of it, or one range), or as UTF-8 text |
| `list(path="/")`, `stat(path)`, `exists(path)` | Entries (name, kind, size), and kind, size and version |
| `write(path, data, *, expected_version=None)` | Write str or bytes, creating parents, and return the new version |
| `mkdir`, `remove`, `copy`, `move` | File operations; base files are copied by reference |
| `changes()` | `Change(path, kind)` for every added, modified or deleted file |

Errors are built-in exceptions (`FileNotFoundError`, `FileExistsError`, `IsADirectoryError`, `NotADirectoryError`, `ValueError`, `OSError`) plus `sandfs.VersionMismatch`, which carries `path`, `expected` and `actual`. An exception raised inside a Python source surfaces as `OSError`. Calls into the core release the GIL, so a slow source never blocks other threads. A workspace is not meant to be shared between threads; fork one per thread instead.

## Mounting a workspace on disk

The `sandfs` command (crate `sandfs-cli`) mounts a workspace as a real directory, so any program can read and write it: a data pipeline, a notebook, a build, an agent's tools. The folder underneath is never modified, and forking never copies it. It mounts on Linux with FUSE 3 (`fusermount3`, package `fuse3`). On macOS it serves the workspace over NFS on localhost and mounts it with macOS's own NFS client, so there is nothing to install. On Windows it does everything but mount, and `sandfs serve --mcp SOURCE` creates workspaces there.

```bash
cargo install --path crates/sandfs-cli
sandfs mount ~/datasets/survey ~/ws/survey       # a host folder, never written to
sandfs fork ~/ws/survey ~/ws/cleanup             # survey's changes so far, without copying the folder
python clean.py ~/ws/cleanup                     # real files, any tool
sandfs changes ~/ws/cleanup                      # A/M/D against the source
sandfs unmount ~/ws/cleanup                      # changes stay saved
sandfs mount cleanup ~/ws/cleanup                # and come back
```

`sandfs serve --mcp NAME` gives an unmounted workspace to an agent as MCP file tools instead, with the source's files under `/mnt`; whatever the agent changes is saved for the next `mount` or `changes`. One process serves a workspace at a time: a second mount or MCP session is refused while the first runs, so parallel attempts each get their own fork.

Changes live in memory and are saved under `$SANDFS_HOME` (default `~/.local/share/sandfs`) once a file has gone a quarter of a second without a write. Closing a file doesn't save it, since the mount is never told about opens and closes; `fsync` saves at once (except on macOS, where it never reaches the mount's NFS server), and `unmount`, `fork` and `changes` save everything first. A fork on disk costs more than a fork in memory. `sandfs fork` never copies the source, but it waits for the workspace's latest changes to be saved, links each changed file into the fork (copying it where the disk can't link), and starts a new mount, which reads the source again rather than sharing what the first mount has read. `git:<path or url>[@rev]` mounts a commit instead of a folder; for a remote repository sandfs fetches only that commit and its folder listings, which takes about a second even for a large repository, and fetches a directory's files in one request when it is first listed. Offline, a commit id that was mounted before still mounts and the folders opened before still read, but any other folder needs the network. A branch or tag name is looked up on the server every time, so it doesn't mount offline.

## What sandfs is not

**Not an execution sandbox.** sandfs does not run commands, and it does not contain the programs that use it. It keeps their writes out of the source, but a program working in a FUSE mount runs with its own process's access and can still read or change anything else its user can. Run untrusted programs in a real sandbox (a container or microVM) and point it at the mount.

**Not a snapshot of a host folder.** A host folder is read when a path below it is first touched. Directory listings and whole-file reads are kept from then on, but ranged reads (which a FUSE mount uses for files over 64 MiB) go back to the folder for every part the kernel hasn't kept in its page cache. If the folder changes while it is mounted, a workspace can show a mix of old and new: a listing or size read before the change next to contents read after it, or two ranges of one file from different versions. Mount a folder nothing else writes to, copy it first, or mount a git commit when the view must not change.

sandfs also has no symlinks and no text-editing or patch tools in the core; those are code concerns that live in the MCP tools, if anywhere. Earlier versions had a bash-like shell, a host-command bridge, a Python executor, access policies and storage adapters. They were dropped to keep the primitive small.

## Repository layout

```
.
├── crates/
│   ├── sandfs-core/    # workspaces, overlay, changes, directory and in-memory sources
│   ├── sandfs-git/     # optional lazy git source
│   ├── sandfs-cli/     # the sandfs command: mount, fork and changes over FUSE
│   ├── sandfs-mcp/     # MCP server: file tools over one workspace
│   └── sandfs-py/      # Python bindings (the sandfs._core extension)
├── docs/               # roadmap, changelog and publishing notes
├── sandfs/             # Python package: re-exports and type stubs
└── tests/              # Python tests
```

## Local development

Building the package compiles the Rust extension, so you need a Rust toolchain ([rustup](https://rustup.rs)). Re-run the install after changing Rust code.

```
python -m venv .venv
source .venv/bin/activate
uv pip install -e .[dev]
pre-commit install
uv run pytest
uv run ruff check .
uv run mypy sandfs
cargo test --workspace
```

## License

MIT

## Further reading

- [Roadmap](https://github.com/lydakis/sandfs/blob/main/docs/roadmap.md)
- [Changelog](https://github.com/lydakis/sandfs/blob/main/docs/changelog.md)
- [Publishing sandfs](https://github.com/lydakis/sandfs/blob/main/docs/publishing.md)
