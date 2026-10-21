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
- A **change** is one file that differs from the sources, as listed by `changes()`. `changes(since=checkpoint)` lists the files that differ from another workspace instead: what an attempt did after a fork you kept, or how two attempts from one fork differ.

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

A source must return the same answer every time, because what it returns is cached and shared between forks. The size in `DirEntry` is optional. A source that pays per request, like an API or a remote store, can also define `prefetch(path, recursive)`: sandfs calls it when a folder's files are about to be read, such as during a search, so the source can fetch them in one trip.

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
sandfs changes cleanup --since survey            # how cleanup differs from survey, file by file
sandfs unmount ~/ws/cleanup                      # changes stay saved, and `sandfs mount cleanup ~/ws/cleanup` brings them back
```

`git:<path or url>[@rev]` mounts a commit instead of a folder, and a URL needs no `git:`, so `sandfs mount https://github.com/owner/repo DIR` is all it takes to look through someone's repository. For a remote repository, sandfs fetches only that commit and its folder listings, so even a large project mounts in about a second. What it fetches later is kept for every workspace of that repository until `sandfs clean` drops it, from repositories not mounted, and deletes what no workspace uses. Listing a folder then fetches that folder's files in one request, since a real folder has to show their sizes:

```bash
sandfs mount https://github.com/pallets/flask@3.1.0 ~/ws/flask
```

The mounted folder has a `.git`, so git itself works in it: `status`, `add`, `commit`, branches and `push`, with commits kept in the workspace and its forks. A mount of a remote repository holds only the mounted commit, not the history before it; a local repository's history is all there. `git status` doesn't read your files, because sandfs tells git which ones changed; on VS Code it takes 0.07 s once the first run (about 10 s on a fresh mount, which lists every folder) is done. A `commit` fetches only the files git compares: the others in each folder that holds a change, or holds a folder that does. On VS Code, a first commit changing two files takes 2.3 s and fetches 0.5 MiB, and later ones take 0.2 s.

`sandfs serve --mcp NAME` hands a saved workspace to an agent as MCP file tools instead of mounting it.

Every mount is served by one background process, started by the first `mount` or `fork` and gone once the last mount is. It detaches from whatever started it: it runs in a session of its own and is adopted by init (or launchd, or the nearest subreaper). So a job runner or shell that kills a job's process group or its process tree leaves other jobs' mounts alone; killing a whole container or cgroup still ends it. It keeps the starting command's environment, which carries what git needs to fetch, except the variables that point git at a repository (`GIT_DIR` and the like).

On Linux, mounting needs FUSE 3 (`fusermount3`, package `fuse3`). On macOS there is nothing to install: the mount is served over NFS on localhost and mounted with macOS's own NFS client. On Windows, `changes`, `list`, `rm` and `serve --mcp` work, and `sandfs serve --mcp SOURCE` gives an agent a workspace there; `fork` and `unmount` need a mount. When changes are saved, what a fork on disk costs and how offline git mounts behave are covered in [sandfs-cli](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-cli/README.md).

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
| `list(path="/")`, `stat(path)`, `exists(path)` | Entries (name, kind, size), and kind, size and version; a kind is `"file"`, `"dir"` or `"symlink"` |
| `write(path, data, *, expected_version=None)` | Write str or bytes, creating parents, and return the new version |
| `mkdir`, `remove`, `copy`, `move` | File operations; copies share the original's bytes |
| `symlink(path, target)`, `read_link(path)` | Make a symbolic link, or read where one points |
| `hard_link(src, dst)`, `hard_links(path)` | Give a file a second name, or list all its names |
| `changes(since=None)` | `Change(path, kind)` for every added, modified or deleted file or link, against the sources or, with `since`, another workspace such as a checkpoint |

Errors are built-in exceptions (`FileNotFoundError`, `FileExistsError`, `IsADirectoryError`, `NotADirectoryError`, `ValueError`, `OSError`) plus `sandfs.VersionMismatch`, which carries `path`, `expected` and `actual`. An exception raised inside a Python source surfaces as `OSError`.

## Performance

Eight attempts at one task run side by side, each on its own fork of a folder holding 10,000 small files and a 256 MiB file. Each one searches every small file, rewrites 50 files, adds 10, deletes 5, checks its work and lists its changes.

| | 8 attempts, wall time | making the 8 forks | peak memory | left on disk |
| --- | ---: | ---: | ---: | ---: |
| sandfs, in process | 0.42 s | 6.6 µs | 157 MiB | 0 |
| sandfs, mounted as folders | 1.4 s | 28 ms | 70 MiB | 4.6 MiB |
| overlayfs (Linux, needs root) | 0.54 s | 27 ms | 7 MiB | 4.4 MiB |
| a full copy per attempt | 5.4 s | 4.1 s | 35 MiB | 3.1 GiB |

In process, every file read is kept in memory once and shared by all the forks, so a second read costs nothing. Mounted folders are all served by one background process, which keeps no copy of a host folder's files, since the kernel already caches them. Measured on a 4-vCPU cloud VM with the data on tmpfs, all rows in one run, on main in October 2026; runs on that VM vary by up to about 1.5 times. To run it yourself, and for the other workloads (cold and warm reads, large files, slow sources), see [sandfs-bench](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-bench/README.md):

```bash
cargo build --release -p sandfs-bench -p sandfs-cli
sudo ./target/release/sandfs-bench --scratch /dev/shm --only workflow
```

### A large repository from a git server

Mounting the latest commit straight from GitHub, against cloning it. "Ready" is the time until files can be read, starting with nothing local:

| | VS Code (20,807 files): ready | on disk | Chromium (510,507 files): ready | on disk |
| --- | ---: | ---: | ---: | ---: |
| `sandfs mount git:URL` | 0.6 s | 10 MiB | 2.8 s | 35 MiB |
| sparse checkout, told in advance which files will be read | 2.8 s | 41 MiB | 9.2 s | 249 MiB |
| shallow clone | 9.4 s | 758 MiB | didn't fit | |
| full clone | 124 s | 2.3 GiB | didn't fit | |

A second copy of the mount (`sandfs fork`) takes 20 ms, against 0.2 to 4.4 s for another git worktree. The cost moves to reading: a file's contents arrive over the network the first time they are needed, and one round trip took about 0.35 s here. On the VS Code mount, with a folder of 992 files fetched fresh, `ls` took 21 ms, `cat` 0.39 s, editing a file 0.38 s, `find` 0.85 s and `grep -r` 3.2 s; a clone answers each in milliseconds because it paid for everything up front. Measured on the same VM through its proxy to GitHub, one run each, so network speed dominates. Rerun with `--only remote --remote URL` and `--only everyday --remote URL`.

## Limits

- **sandfs is not an execution sandbox.** It never runs commands, and it doesn't contain the programs that use it. It keeps their writes out of your sources, but a program working in a mounted folder can still read or change anything else its user can. Run untrusted programs in a container or microVM pointed at the mount.
- **Git runs what a mount's `.git` settings name.** Git settings can name programs for git to run, and anything that can write to a mounted folder can change its `.git`. Don't run git yourself in a mounted folder that a program you don't trust can write to, just as with any folder someone else can change. Over MCP, sandfs refuses changes to a `git:` mount's `.git`; a host folder that holds its own `.git` gets no such protection, so treat an agent's changes there the same way.
- **A mounted folder isn't frozen.** Folders are read lazily, so if one changes while it's mounted, a workspace can show a mix of old and new. Mount a folder nothing else writes to, copy it first, or mount a git commit when the view must not change.
- **Symbolic links are kept, never followed.** A link is stored as its target, and a host folder's links are listed as links. In a mounted folder the system resolves them, as on any filesystem; the library and the MCP tools never do, so `read` refuses a link and `read_link` gives its target. In a mount, a host folder's link into the folder reaches the workspace's copy, and one out of it reaches the host itself, where a link of the host's own can still lead back into the folder. A git commit's links aren't shown yet.
- **Hard links work in the library and in a Linux mount**, where every name of a file is one inode; the macOS mount can't make them yet, so tools there fall back to copying. `changes()` lists each name of a linked file on its own. Sockets, devices and fifos aren't supported.

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
