# sandfs

`sandfs` is the filesystem you give an agent instead of a bash tool. Mount a git commit or a directory without copying it, let the agent read and edit through a copy-on-write workspace, fork that workspace in O(1) for every attempt, and collect a diff of what changed.

> **Status**: early. The core is Rust (`crates/`), and the Python package is a thin binding over it. APIs may still change.

## Primitives

- **Source**: a read-only tree, such as a git commit, a host directory or in-memory files. It is read only when a path below it is touched.
- **Workspace**: sources mounted at paths, plus a copy-on-write overlay that holds every write, directory and deletion. Sources are never modified.
- **Change**: one file that differs from the mounts, returned by `diff()`.

Everything else is a composition. A snapshot is a fork nobody writes to, and a worktree is a fork mounted at a path.

## Installation

```bash
pip install git+https://github.com/lydakis/sandfs.git   # builds the Rust extension; needs a Rust toolchain
```

## Quickstart

```python
import sandfs

base = sandfs.Workspace()
base.mount_git("/repo", "path/to/clone", rev="main")   # no checkout; files read on demand
base.mount_dir("/data", "./fixtures")                   # host directory, never written to
base.mount_files("/notes", {"todo.md": "- ship it\n"})

attempt = base.fork()                                   # one fork per agent or attempt
attempt.edit("/repo/README.md", "sandfs", "SandFS")
version = attempt.write("/notes/plan.md", "1. fork\n2. diff\n")
attempt.write("/notes/plan.md", "revised", expected_version=version)

for change in attempt.diff():
    print(change.kind, change.path)                     # added /notes/plan.md, modified /repo/README.md
```

`base` is unchanged: forks share the mounts and everything already read, but never see each other's changes.

In a partial clone (`git clone --filter=blob:none`), git fetches a file's contents from the remote only when it is first read, so mounting a large repository costs its trees plus whatever the agent opens.

## API

| Call | Does |
| --- | --- |
| `mount_git(path, repo, rev="HEAD")` | Mount a commit, tag or branch of a local repository, resolved once |
| `mount_dir(path, root)` | Mount a host directory read-only |
| `mount_files(path, {name: str or bytes})` | Mount in-memory files read-only |
| `fork()` | A new workspace with the same mounts and changes, in O(1) |
| `read(path)`, `read_text(path)` | File contents as bytes or UTF-8 text, following a final symlink |
| `list(path="/")`, `stat(path)`, `exists(path)`, `read_link(path)` | Entries (name, kind, size), kind, size and version |
| `write(path, data, *, expected_version=None)` | Write str or bytes, creating parents, and return the new version |
| `edit(path, old, new, *, replace_all=False, expected_version=None)` | Exact-string replacement; `old` must match once unless `replace_all` |
| `mkdir`, `symlink`, `remove`, `copy`, `move` | File operations; base files are copied by reference |
| `diff()` | `Change(path, kind)` for every added, modified or deleted file |

Errors are built-in exceptions (`FileNotFoundError`, `FileExistsError`, `IsADirectoryError`, `NotADirectoryError`, `ValueError`, `OSError`) plus `sandfs.VersionMismatch`, which carries `path`, `expected` and `actual`, and `sandfs.EditFailed`. Calls that may read a source release the GIL. A workspace is not meant to be shared between threads; fork one per thread instead.

## What sandfs is not

sandfs does not run code. Commands an agent wants to run belong in a real sandbox (a container or microVM) pointed at the files, and a FUSE mount of a workspace is on the roadmap for that. The 0.3 releases had a bash-like shell, a host-command bridge, a Python executor, access policies and storage adapters. They were dropped to keep the primitive small; 0.3 remains in git history.

## Repository layout

```
.
├── crates/
│   ├── sandfs-core/    # workspaces, sources, overlay, diff
│   ├── sandfs-git/     # lazy git commit mounts
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
