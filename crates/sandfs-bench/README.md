# sandfs-bench

Benchmarks that run the same file workloads on sandfs and on what it replaces, so every optimization can be measured against a fixed baseline. Not published.

## Running

```bash
cargo build --release -p sandfs-bench -p sandfs-cli   # the FUSE target uses the sandfs binary
sudo ./target/release/sandfs-bench --scratch /dev/shm  # root lets it mount overlayfs; FUSE needs fuse3
./target/release/sandfs-bench --scale quick            # a few seconds per target
./target/release/sandfs-bench --only lazy/ --latency-us 20000
uv run python crates/sandfs-bench/python/bench.py --scratch /dev/shm   # the Python bindings
./target/release/sandfs-bench --only remote --remote https://github.com/microsoft/vscode --scratch /dev/shm
./target/release/sandfs-bench --only everyday --remote https://github.com/microsoft/vscode --scratch /dev/shm
./target/release/sandfs-bench --only build --scratch /dev/shm
```

Put the scratch directory on tmpfs (`/dev/shm`) when comparing numbers. sandfs keeps its changes in memory, so a disk under the other targets mostly measures the disk, and writeback makes their write and copy timings swing by several times between runs.

`--targets host,sandfs,overlayfs,fuse` picks targets (default: every one available), `--only` filters workloads by name, `--json FILE` keeps every sample, and `--scratch DIR` sets where the data set and mounts go (default: the system temp directory). Targets that cannot run here are listed as skipped.

## Targets

| target | what it is | fork | changes |
|---|---|---|---|
| host | the data set read and written directly with `std::fs`: the floor | copy the tree | compare the copy with the original, file by file |
| sandfs | a `Workspace` with the data set mounted from a `DirSource` | `Workspace::fork` | `Workspace::changes` |
| overlayfs | Linux's copy-on-write filesystem, data set as the lower layer (needs root) | copy the upper layer, mount a new overlay | walk the upper layer |
| sandfs FUSE | `sandfs mount` of the data set as a host folder | `sandfs fork` | `sandfs changes` |

The FUSE fork and changes run the `sandfs` command, so they include starting a process, the way a harness calls them.

## Data and workloads

The data set is generated once per run from a fixed seed, and it is not code: arbitrary bytes in four shapes. At the default scale: 10,000 files of 1 to 16 KiB in 100 directories, one directory of 10,000 files, a chain of 64 nested directories, and a 256 MiB file.

Every timed run starts from a fresh view (a new workspace or a new mount), so sandfs's caches start empty while the kernel's page cache is warm for every target alike. A workload's untimed setup runs first on that view; tearing a fork down (an unmount) is not timed. Each workload checks its result (bytes read, files listed, changes found, and after the timer stops, that every write and delete landed), so a target that silently does less fails instead of looking fast. The host fork copies the whole data set, large file included.

| workload | timed operation |
|---|---|
| small/read-cold | read every small file once |
| small/read-warm | read them all again |
| small/stat | stat every small file, after listing their directories |
| small/walk | list every directory of the small-file tree |
| wide/list | list the 10,000-entry directory, already read once |
| deep/stat | stat the file at the bottom of the chain 10,000 times |
| large/read-whole | read the large file whole |
| large/read-ranges | read 1,000 random 64 KiB ranges of it |
| write/create, overwrite, delete | 1,000 writes of 4 KiB, or 1,000 deletes |
| fork/clean, fork/changed | fork a view without changes, or with 2,000 changed files |
| changes | list the changes after 1,000 creates, 1,000 overwrites and 100 deletes |

The lazy-source table mounts 1,000 of the small files from a source that sleeps for a fixed delay per call (1 ms by default), like an API or an object store, and compares reading them through sandfs with fetching everything up front.

## Workflow

`--only workflow` runs a whole task several times side by side (`--attempts 8` by default), the way a harness runs parallel attempts at one job. Each attempt gets its own fork of one prepared view, then searches every small file for a byte pattern, rewrites the 50 files it picked (matches first), adds 10 notes and deletes 5, checks its work by running `cksum` over two directories and the rewritten files, and lists its changes. Forks are made one after another; the attempts then run on a thread each. On targets that are real directories the check is a separate process reading through the filesystem; through the Rust API the same checksums are computed in-process. Every checksum is compared with the contents the attempt should have left, so a target that writes the wrong bytes fails the run.

Besides wall time and the median of each step, the table shows peak memory and the bytes the attempts left in the scratch directory. Each target's workflow runs in a fresh process, so its memory peak holds only its own work: that process plus any serving processes, each counted whole, so shared pages are counted more than once. A host fork is a full copy, so it pays in time and disk for every attempt; the FUSE mount saves each fork's changes to disk when they are listed.

## Everyday operations

`--only everyday` runs what an agent or a person does in a folder, one operation at a time, each on a fresh view so sandfs's caches start empty:

| operation | what it does |
|---|---|
| ls, ls -l | list a folder, then also the size of every entry |
| find | list every folder below one |
| stat, cat | the size of the deepest file, and reading a file |
| grep -r | read every file below a folder and count those holding a byte pattern |
| create, edit, mv, rm | write a new file, insert 16 bytes in the middle of one, move one, delete one |
| changes | list the changes after create, edit, mv and rm |

It runs them on three kinds of source, by every way of reaching each one here:

| source | columns |
|---|---|
| the data set as a host folder | the host directly (the floor), sandfs through the Rust API, the `sandfs` mount |
| a source taking `--latency-us` per call (1 ms by default), holding the first 1,000 small files | sandfs through the Rust API |
| the `--remote` repository, with nothing fetched before each operation | sandfs through the Rust API (a `GitSource` on a new blobless copy), the `sandfs` mount (with a `SANDFS_HOME` of its own) |

The Rust API is what the Python package and the MCP server call, so its column stands for them too. Every column must agree on each operation's result (entries listed, bytes read, changes found). The remote operations run once each, since each needs a fresh copy.

## Remote repository

`--remote URL` adds a table for a large repository on a git server (it needs the network; `--only remote` runs just that table). Each method starts with nothing local, gets the commit at the server's `HEAD`, reads one file, reads 20 more spread evenly across the tree, searches one folder (reads every file in it and counts those holding a fixed byte pattern), and makes a second copy to work in:

| method | how it gets the files | second copy |
|---|---|---|
| sandfs mount | `sandfs mount git:URL@COMMIT`: the commit and its folder listings, then each folder's files when it is first listed | `sandfs fork` |
| sparse checkout | a depth-1 blobless fetch, then a checkout of exactly the files the run will read, listed in advance: git's best case, which needs to know what will be read | `git worktree add` |
| shallow clone | a depth-1 fetch of the commit with every file, checked out | `git worktree add` |
| blobless clone | `git clone --filter=blob:none`: all history without file contents, then a checkout of every file | `git worktree add` |
| full clone | `git clone`, checked out | `git worktree add` |

Each copy then has a day of work done in it: the first `git status`, a commit of one changed file, `git status` again, and `grep -r` over the whole copy (stopped and reported after 10 minutes).

`--remote-folder` picks the folder to search (default: the largest folder of at most 1,000 files) and `--remote-methods` the methods (default: all). Each method runs once, in its own process, so its peak memory is the largest command it ran plus, for sandfs, its serving processes; "on disk" is everything it left in the scratch directory. Every method must read the same bytes and find the same matches. Timings depend on the network, so compare methods within one run rather than across runs.

## Build

`--only build` runs a program that writes a lot: `cargo build` (debug) of one of this repository's packages (`--build-package`, default `sandfs-cli`), with the build output inside the folder, once in a copy of the repository on the host and once in a sandfs mount of another copy. Dependencies are fetched first, so it needs the network once. The first build writes every compiled file; each rebuild changes one source file first and then rereads what the first build wrote and rewrites part of it, the way a person or an agent rebuilds after an edit. The table shows the first build, the median rebuild, and the memory of the process serving the mount afterwards.
