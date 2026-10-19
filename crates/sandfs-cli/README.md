# sandfs-cli

The `sandfs` command: mount a [sandfs](https://crates.io/crates/sandfs-core) workspace as a real directory with FUSE, so any program can read and write it. The source underneath, such as a host folder, is never modified; every write lands in a copy-on-write overlay. Forking never copies the source, and `changes` lists the files a workspace added, modified or deleted.

It needs Linux with FUSE 3 (`fusermount3`, package `fuse3`). Install it with `cargo install sandfs-cli`.

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

`sandfs serve --mcp NAME` hands an unmounted workspace to an agent as MCP file tools over stdin and stdout (read, list, glob, grep, write, edit, move, delete, changes), with the source's files under `/mnt`. Each change is saved before its reply, so a later `mount` or `changes` sees it.

Changes live in memory and are saved under `$SANDFS_HOME` (default `~/.local/share/sandfs`) shortly after each write. A fork on disk costs more than a fork in memory. `sandfs fork` never copies the source, but it waits for the workspace's latest changes to be saved, links each changed file into the fork (copying it where the disk can't link), and starts a new mount, which reads the source again rather than sharing what the first mount has read. A host folder is read as it is when each path is first touched, so mount one nothing else is changing. `git:<path or url>[@rev]` mounts a commit instead of a folder; for a remote repository sandfs keeps a partial clone and fetches file contents when they are first read.

See the [repository](https://github.com/lydakis/sandfs) for the design.

License: MIT
