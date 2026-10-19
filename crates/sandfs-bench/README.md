# sandfs-bench

Benchmarks that run the same file workloads on sandfs and on what it replaces, so every optimization can be measured against a fixed baseline. Not published.

## Running

```bash
cargo build --release -p sandfs-bench -p sandfs-cli   # the FUSE target uses the sandfs binary
sudo ./target/release/sandfs-bench --scratch /dev/shm  # root lets it mount overlayfs; FUSE needs fuse3
./target/release/sandfs-bench --scale quick            # a few seconds per target
./target/release/sandfs-bench --only lazy/ --latency-us 20000
uv run python crates/sandfs-bench/python/bench.py --scratch /dev/shm   # the Python bindings
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
