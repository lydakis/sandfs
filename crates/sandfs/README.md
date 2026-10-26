# sandfs

Lazy, copy-on-write workspaces over host folders and git repositories, saved on disk and mounted as real directories: FUSE on Linux, and on macOS an NFS server on localhost that macOS's own client mounts, so nothing needs installing there. This is the library behind the [`sandfs` command](https://crates.io/crates/sandfs-cli), for a program that wants its workspaces without running it.

```rust,no_run
fn main() -> anyhow::Result<()> {
    // First thing in `main`: sandfs runs this program again to serve mounts.
    sandfs::handle_relaunch(&["sandfs"]);
    let home = sandfs::Home::locate()?;
    let repo = home.mount("https://github.com/lydakis/sandfs", "/tmp/sandfs".as_ref(), &Default::default())?;
    let task = home.fork(&repo.name, Some("/tmp/task".as_ref()), &Default::default())?;
    std::fs::write("/tmp/task/NOTES.md", "a task's notes\n")?;
    for change in home.changes(&task.name, &Default::default())?.changes {
        println!("{:?} {}", change.kind, change.path);
    }
    home.update(&task.name, None)?; // onto the newest commit, in place
    home.remove(&task.name, true)?;
    Ok(())
}
```

Each command of `sandfs` is a method of `Home`: `mount`, `fork`, `changes` (with `copy_to`), `update`, `workspaces`, `unmount`, `remove`, `clean`, `serve_mcp` and `shell`. They return what they found instead of printing it. The [sandfs-cli README](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-cli/README.md) says what each does in detail.

## The mount process

A mount is served by one background process per home, which outlives the program that asked for it. sandfs starts that process by running the current program again, with the words given to `handle_relaunch` followed by its own (`daemon --serve`); git's file monitor in a git mount and `shell` on macOS run it the same way. So `handle_relaunch` must come first in `main`: in those runs it does what they were started for and exits. A program with commands of its own gives a prefix none of them uses, such as `&["sandfs"]`; the `sandfs` command gives none.

The home is `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else `~/.local/share/sandfs`, shared with the `sandfs` command. Without the default `mount` feature (and on Windows) everything but mounting works.

[`sandfs-core`](https://crates.io/crates/sandfs-core) is the in-memory engine underneath: sources, O(1) forks and changes, with nothing on disk.

## License

MIT, except `src/nfsserve`, which is from the nfsserve crate under BSD-3-Clause (see its LICENSE).
