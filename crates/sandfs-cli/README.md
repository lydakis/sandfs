# sandfs-cli

The `sandfs` command: mount a [sandfs](https://crates.io/crates/sandfs-core) workspace as a real directory, so any program can read and write it. The source underneath, such as a host folder, is never modified; every write lands in a copy-on-write overlay. Forking never copies the source, and `changes` lists the files a workspace added, modified or deleted.

It mounts on Linux with FUSE 3 (`fusermount3`, package `fuse3`). On macOS the mount's process serves the workspace over NFS on localhost and mounts it with macOS's own NFS client, so nothing needs installing or approving. Like a FUSE mount, it answers only the user who mounted it (and root). Install it with `cargo install sandfs-cli`. On Windows you get everything but mounting: `serve --mcp`, `changes`, `list` and `rm`, with `sandfs serve --mcp SOURCE` creating a workspace over a source.

```bash
sandfs mount ~/datasets/survey ~/ws/survey       # a host folder, never written to
sandfs fork ~/ws/survey ~/ws/cleanup             # survey's changes so far, without copying the folder
python clean.py ~/ws/cleanup                     # real files, any tool
sandfs changes ~/ws/cleanup                      # A/M/D against the source
sandfs unmount ~/ws/cleanup                      # changes stay saved
sandfs mount cleanup ~/ws/cleanup                # and come back
sandfs list                                      # workspaces and where they are mounted
sandfs rm cleanup                                # forget a workspace and its changes
```

`sandfs serve --mcp NAME` hands an unmounted workspace to an agent as MCP file tools over stdin and stdout (read, list, glob, grep, write, edit, move, delete, changes), with the source's files under `/mnt`. Each change is saved before its reply, so a later `mount` or `changes` sees it, and a change that can't be saved is undone and reported as an error.

One process serves a workspace at a time. A mount or an MCP session holds it until it ends, a second one is refused with the holder's name, and a crashed holder releases it. Parallel attempts each get their own fork.

Changes live in memory and are saved under `$SANDFS_HOME` (default `~/.local/share/sandfs`) once a file has gone a quarter of a second without a write. Closing a file doesn't save it, since the mount is never told about opens and closes; `fsync` saves at once (except on macOS, where it never reaches the mount's NFS server), and `unmount`, `fork` and `changes` save everything first. A fork on disk costs more than a fork in memory. `sandfs fork` never copies the source, but it waits for the workspace's latest changes to be saved, links each changed file into the fork (copying it where the disk can't link), and starts a new mount, which reads the source again rather than sharing what the first mount has read. A host folder is read as it is when each path is first touched, so mount one nothing else is changing. `git:<path or url>[@rev]` mounts a commit instead of a folder; for a remote repository sandfs fetches only that commit and its folder listings, with no history, and fetches file contents when they are first read. Offline, a commit id that was mounted before still mounts and the folders opened before still read; a branch or tag name is looked up on the server every time, so it doesn't mount offline.

See the [repository](https://github.com/lydakis/sandfs) for the design.

License: MIT
