# sandfs

`sandfs` is a sandbox filesystem for agents. Mount any data without copying it (a folder, in-memory files, an API, a bucket, a git commit), let the agent read and write through a copy-on-write workspace, fork that workspace in O(1) for every attempt, and collect the files it changed. Files are bytes of any kind: documents, spreadsheets, images, data sets and code alike.

> **Status**: early. The core is Rust (`crates/`), and the Python package is a thin binding over it. APIs may still change.

## Primitives

- **Source**: a read-only tree, read only when a path below it is touched. sandfs ships a host directory, in-memory files and an optional git source; anything else is two methods you write, `list` and `read`.
- **Workspace**: sources mounted at paths, plus a copy-on-write overlay that holds every write, directory and deletion. Sources are never modified.
- **Change**: one file that differs from the mounts, returned by `changes()`.

Everything else is a composition. A snapshot is a fork nobody writes to, a working copy on disk is a fork mounted over FUSE, and saving is the harness walking `changes()` and writing each changed file wherever its data lives.

## Installation

```bash
pip install git+https://github.com/lydakis/sandfs.git   # builds the Rust extension; needs a Rust toolchain
```

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

The `sandfs` command (crate `sandfs-cli`) mounts a workspace as a real directory with FUSE, so any program can read and write it: a data pipeline, a notebook, a build, an agent's tools. The folder underneath is never modified, and forking is O(1). It needs Linux with FUSE 3 (`fusermount3`, package `fuse3`).

```bash
cargo install --path crates/sandfs-cli
sandfs mount ~/datasets/survey ~/ws/survey       # a host folder, never written to
sandfs fork ~/ws/survey ~/ws/cleanup             # O(1): a copy with survey's changes so far
python clean.py ~/ws/cleanup                     # real files, any tool
sandfs changes ~/ws/cleanup                      # A/M/D against the source
sandfs unmount ~/ws/cleanup                      # changes stay saved
sandfs mount cleanup ~/ws/cleanup                # and come back
```

A workspace keeps its source from when it was created. Changes live in memory and are saved under `$SANDFS_HOME` (default `~/.local/share/sandfs`) shortly after each write. `git:<path or url>[@rev]` mounts a commit instead of a folder; for a remote repository sandfs keeps a partial clone and fetches a directory's files in one request when it is first listed.

## What sandfs is not

sandfs does not run commands. Commands an agent wants to run belong in a real sandbox (a container or microVM) pointed at the files, for example through a FUSE mount of a workspace. It also has no symlinks and no text-editing or patch tools in the core; those are code concerns that live in the MCP tools, if anywhere. The 0.3 releases had a bash-like shell, a host-command bridge, a Python executor, access policies and storage adapters. They were dropped to keep the primitive small; 0.3 remains in git history.

## Repository layout

```
.
├── crates/
│   ├── sandfs-core/    # workspaces, overlay, changes, directory and in-memory sources
│   ├── sandfs-git/     # optional lazy git source
│   ├── sandfs-cli/     # the sandfs command: mount, fork and changes over FUSE
│   └── sandfs-py/      # Python bindings (the sandfs._core extension)
├── docs/               # roadmap and publishing notes
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

- [Roadmap](docs/roadmap.md)
- [Publishing sandfs](docs/publishing.md)
