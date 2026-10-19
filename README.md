# sandfs

sandfs gives an agent a private copy of your files to work in, without copying anything. Point it at a folder, a git commit or any data you can list and read. The agent reads and writes as it likes, your originals never change, and when it's done you get the list of files it added, changed or deleted, so you decide what to keep.

- **Nothing is copied up front.** Files are fetched from their source as they are needed, so mounting a huge folder or repository is instant.
- **Every attempt gets its own copy, for free.** Forking a workspace in Python or Rust takes constant time however much data is mounted (a fork mounted as a folder costs a few milliseconds more), so ten agents can try ten things side by side without seeing each other's changes.
- **You see exactly what changed.** `changes()` lists the files, and you write back only the ones you want.

Files are bytes of any kind: documents, spreadsheets, images, data sets and code alike.

> **Status**: early. The core is Rust, the Python package is a thin binding over it, and APIs may still change.

## Install

```bash
pip install sandfs             # Python 3.11+, prebuilt for Linux, macOS and Windows
```

## A first workspace

```python
import pathlib
import sandfs

pathlib.Path("survey").mkdir(exist_ok=True)          # a small folder to try it on
pathlib.Path("survey/responses.csv").write_text("id,answer\n1,yes\n")
pathlib.Path("survey/raw.txt").write_text("draft\n")

base = sandfs.Workspace()
base.mount("/survey", sandfs.DirSource("./survey"))   # a folder on disk, never written to

attempt = base.fork()                                # a private copy, made instantly
names = [entry.name for entry in attempt.list("/survey")]
attempt.write("/survey/index.md", "".join(f"- {name}\n" for name in names))
attempt.remove("/survey/raw.txt")

for change in attempt.changes():
    print(change.kind, change.path)                  # added /survey/index.md, deleted /survey/raw.txt
```

`./survey` on disk and `base` are untouched: the write and the delete exist only in `attempt`. To keep the results, copy each added or modified file out of `attempt` and delete each deleted path from wherever you keep them.

That is the whole model:

- A **source** is read-only data mounted at a path: a folder, a git commit, files held in memory, or your own data.
- A **workspace** is sources mounted at paths, plus a layer that holds every write and delete. `fork()` copies a workspace in constant time.
- A **change** is one file that differs from the sources, as listed by `changes()`.

## What you can mount

### A git commit

Mount any commit of a repository without checking it out. In a partial clone, git downloads a file's contents only when the file is first read or its size is first asked for with `stat`.

```python
# git clone --filter=blob:none --no-checkout https://github.com/pallets/flask
base = sandfs.Workspace()
base.mount("/flask", sandfs.GitSource("./flask", rev="3.1.0"))

attempt = base.fork()
text = attempt.read_text("/flask/README.md")
attempt.write("/flask/README.md", text.replace("Flask", "Flask (edited)", 1))

for change in attempt.changes():
    print(change.kind, change.path)                  # modified /flask/README.md
```

The `sandfs` command can mount a remote repository directly; see [a real folder on disk](#a-real-folder-on-disk).

### Files held in memory

```python
base.mount("/brief", sandfs.MemorySource({"task.md": "Summarise the survey\n", "logo.png": png_bytes}))
```

### Your own data

Any object with two methods is a source: `list(path)` returns the entries of a directory and `read(path)` returns a file's bytes. sandfs calls them only when the agent looks.

```python
class Tickets:
    """Support tickets from an API, one JSON file each."""

    def list(self, path):
        return [sandfs.DirEntry(f"{t.id}.json", size=t.size) for t in api.tickets()]

    def read(self, path):
        return api.ticket_json(path.removeprefix("/").removesuffix(".json"))

base.mount("/tickets", Tickets())
```

A source must return the same answer every time, because what it returns is cached and shared between forks. The size in `DirEntry` is optional.

## Giving a workspace to an agent

`sandfs-mcp` is an MCP server that gives an agent file tools over one workspace instead of a shell: `read`, `list`, `glob`, `grep`, `write`, `edit`, `move`, `delete` and `changes`. Point any MCP client at it:

```bash
cargo install sandfs-mcp
```

```json
{ "mcpServers": { "sandfs": {
    "command": "sandfs-mcp",
    "args": ["--dir", "/data=./datasets", "--dir", "/notes=./notes"]
} } }
```

Each `--dir` mounts a folder, read lazily and never written to. A harness that builds its own workspace can embed the server instead; see [sandfs-mcp](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-mcp/README.md).

## A real folder on disk

The `sandfs` command mounts a workspace as an ordinary directory, so any program can use it: a notebook, a data pipeline, a build, an agent's own tools.

```bash
cargo install sandfs-cli
sandfs mount ~/datasets/survey ~/ws/survey       # the folder underneath is never written to
sandfs fork ~/ws/survey ~/ws/cleanup             # survey's changes so far, without copying the folder
python clean.py ~/ws/cleanup                     # real files, any tool
sandfs changes ~/ws/cleanup                      # A/M/D for each changed file
sandfs unmount ~/ws/cleanup                      # changes stay saved, and `sandfs mount cleanup ~/ws/cleanup` brings them back
```

`git:<path or url>[@rev]` mounts a commit instead of a folder. For a remote repository, sandfs fetches only that commit and its folder listings, so even a large project mounts in about a second. Listing a folder then fetches that folder's files in one request, since a real folder has to show their sizes, and when the folder holds at most 1,000 files in all, that request brings every file under it too:

```bash
sandfs mount git:https://github.com/pallets/flask@3.1.0 ~/ws/flask
```

`sandfs serve --mcp NAME` hands a saved workspace to an agent as MCP file tools instead of mounting it.

Mounting needs Linux with FUSE 3 (`fusermount3`, package `fuse3`). On macOS and Windows, `changes`, `list`, `rm` and `serve --mcp` work, and `sandfs serve --mcp SOURCE` gives an agent a workspace there; `fork` and `unmount` need a mount. When changes are saved, what a fork on disk costs and how offline git mounts behave are covered in [sandfs-cli](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-cli/README.md).

## More features

- **Large files**: `read(path, offset=..., length=...)` reads one range without caching the file. A folder source seeks on disk, so a multi-gigabyte data set or video is never held in memory whole. A Python source can define `read_range(path, offset, length)` to do the same; without it, and for a git commit, the whole file is read and then sliced.
- **Safe concurrent writes**: `stat(path).version` and `write(...)` give a file's version, and `write(path, data, expected_version=v)` raises `sandfs.VersionMismatch` if the file changed since.
- **Threads**: calls release the GIL, so a slow source never blocks other threads. Use one workspace per thread; forks are cheap.
- **Rust**: the same calls are in the `sandfs-core` crate (`cargo add sandfs-core`), with git in `sandfs-git`.

### Python API

| Call | Does |
| --- | --- |
| `mount(path, source)` | Mount a `DirSource(root)`, `MemorySource({name: str or bytes})`, `GitSource(repo, rev)`, or any object with `list(path)` and `read(path)` |
| `fork()` | A new workspace with the same mounts and changes, in constant time |
| `read(path, *, offset=0, length=None)`, `read_text(path)` | File contents as bytes (all of it, or one range), or as UTF-8 text |
| `list(path="/")`, `stat(path)`, `exists(path)` | Entries (name, kind, size), and kind, size and version |
| `write(path, data, *, expected_version=None)` | Write str or bytes, creating parents, and return the new version |
| `mkdir`, `remove`, `copy`, `move` | File operations; copies share the original's bytes |
| `changes()` | `Change(path, kind)` for every added, modified or deleted file |

Errors are built-in exceptions (`FileNotFoundError`, `FileExistsError`, `IsADirectoryError`, `NotADirectoryError`, `ValueError`, `OSError`) plus `sandfs.VersionMismatch`, which carries `path`, `expected` and `actual`. An exception raised inside a Python source surfaces as `OSError`.

## Performance

Eight attempts at one task run side by side, each on its own fork of a folder holding 10,000 small files and a 256 MiB file. Each one searches every small file, rewrites 50 files, adds 10, deletes 5, checks its work and lists its changes.

| | 8 attempts, wall time | making the 8 forks | peak memory | left on disk |
| --- | ---: | ---: | ---: | ---: |
| sandfs, in process | 0.45 s | 4.5 µs | 142 MiB | 0 |
| sandfs, mounted as folders | 1.9 s | 55 ms | 780 MiB | 4.6 MiB |
| overlayfs (Linux, needs root) | 0.59 s | 36 ms | 7 MiB | 4.4 MiB |
| a full copy per attempt | 6.0 s | 4.7 s | 36 MiB | 3.1 GiB |

Each mounted fork is its own process with its own cache today, which is why mounted folders use the most memory. Measured on a 4-vCPU cloud VM with the data on tmpfs, on main shortly after 0.4.0 (October 2026); runs on that VM vary by up to about 1.5 times. To run it yourself, and for the other workloads (cold and warm reads, large files, slow sources), see [sandfs-bench](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-bench/README.md):

```bash
cargo build --release -p sandfs-bench -p sandfs-cli
sudo ./target/release/sandfs-bench --scratch /dev/shm --only workflow
```

## Limits

- **sandfs is not an execution sandbox.** It never runs commands, and it doesn't contain the programs that use it. It keeps their writes out of your sources, but a program working in a mounted folder can still read or change anything else its user can. Run untrusted programs in a container or microVM pointed at the mount.
- **A mounted folder isn't frozen.** Folders are read lazily, so if one changes while it's mounted, a workspace can show a mix of old and new. Mount a folder nothing else writes to, copy it first, or mount a git commit when the view must not change.
- **No symlinks.** A folder source skips them, along with sockets and devices.

## Contributing

Building from source compiles the Rust extension, so you need a Rust toolchain ([rustup](https://rustup.rs)).

```bash
uv venv
uv pip install -e .[dev]     # re-run after changing Rust code
uv run pytest
cargo test --workspace
```

[AGENTS.md](https://github.com/lydakis/sandfs/blob/main/AGENTS.md) has the repository layout and the checks to run before opening a pull request. See also the [roadmap](https://github.com/lydakis/sandfs/blob/main/docs/roadmap.md), the [changelog](https://github.com/lydakis/sandfs/blob/main/docs/changelog.md) and [how releases are published](https://github.com/lydakis/sandfs/blob/main/docs/publishing.md).

## License

MIT
