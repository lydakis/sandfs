# sandfs-cli

The `sandfs` command: mount a [sandfs](https://crates.io/crates/sandfs-core) workspace as a real directory with FUSE, so any program can read and write it. The source underneath, such as a host folder, is never modified; every write lands in a copy-on-write overlay. Forking is O(1), and `changes` lists the files a workspace added, modified or deleted.

It needs Linux with FUSE 3 (`fusermount3`, package `fuse3`). Install it with `cargo install sandfs-cli`.

```bash
sandfs mount ~/datasets/survey ~/ws/survey       # a host folder, never written to
sandfs fork ~/ws/survey ~/ws/cleanup             # O(1): a copy with survey's changes so far
python clean.py ~/ws/cleanup                     # real files, any tool
sandfs changes ~/ws/cleanup                      # A/M/D against the source
sandfs unmount ~/ws/cleanup                      # changes stay saved
sandfs mount cleanup ~/ws/cleanup                # and come back
sandfs list                                      # workspaces and where they are mounted
sandfs rm cleanup                                # forget a workspace and its changes
```

`sandfs serve --mcp NAME` hands an unmounted workspace to an agent as MCP file tools over stdin and stdout (read, list, glob, grep, write, edit, move, delete, changes), with the source's files under `/mnt`. Each change is saved before its reply, so a later `mount` or `changes` sees it.

A workspace keeps its source from when it was created. Changes are saved under `$SANDFS_HOME` (default `~/.local/share/sandfs`) shortly after each write. `git:<path or url>[@rev]` mounts a commit instead of a folder; for a remote repository sandfs keeps a partial clone and fetches file contents when they are first read.

See the [repository](https://github.com/lydakis/sandfs) for the design.

License: MIT
