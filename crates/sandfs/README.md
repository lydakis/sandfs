# sandfs

Copies of folders that cost nothing to make, saved on disk and mounted as real folders any program can work in. This is the library behind the [`sandfs` command](https://crates.io/crates/sandfs-cli), for a program of your own that wants those copies without asking anyone to install or run `sandfs`: a tool that tries a cleanup of someone's files before touching them, a job runner that gives each job its own copy of a shared data folder, a harness that hands each agent its own folder and reviews what it changed.

- **A workspace is a folder over a source, read lazily and never written to.** The source is a folder on disk, a commit of a git repository (a remote one is fetched a file at a time, as files are read), or a frozen copy of a plain folder.
- **Forks cost nothing up front.** A fork of a workspace has its changes so far and none of the source copied, so a hundred copies of a large folder take milliseconds each and no disk until something is written.
- **You get the list of what changed.** `changes` gives the files a workspace added, modified or deleted, against its source or another workspace, and can write them into a folder, refusing to overwrite a file edited there in the meantime.
- **A workspace can move onto a newer source,** such as a folder's new contents or a branch's newest commit, keeping its own changes, even while it is mounted.
- **Mounts need nothing installed on macOS.** Linux mounts with FUSE 3; macOS with an NFS server on localhost that macOS's own client mounts, with no kernel extension.

```bash
cargo add sandfs anyhow
```

```rust,no_run
fn main() -> anyhow::Result<()> {
    // First thing in `main`: sandfs runs this program again to serve mounts.
    sandfs::handle_relaunch(&[]);
    let home = sandfs::Home::locate()?;

    // A folder of photos, mounted at /tmp/photos; /srv/photos itself is never written to.
    let photos = home.mount("/srv/photos", "/tmp/photos".as_ref(), &Default::default())?;
    // A copy for one job, mounted at /tmp/job-1.
    let job = home.fork(&photos.name, Some("/tmp/job-1".as_ref()), &Default::default())?;

    // Any program can work in /tmp/job-1; here, this one.
    std::fs::write("/tmp/job-1/captions.txt", "beach, 2019\n")?;

    for change in home.changes(&job.name, &Default::default())?.changes {
        println!("{:?} {}", change.kind, change.path); // Added captions.txt
    }
    home.remove(&job.name, true)?; // unmount the copy and delete it
    Ok(())
}
```

Each command of `sandfs` is a method of `Home`: `mount`, `fork`, `changes` (with `copy_to`), `update`, `workspaces`, `unmount`, `remove`, `clean`, `serve_mcp` and `shell`. They return what they found instead of printing it. The [docs](https://docs.rs/sandfs) cover each one, and the [sandfs-cli README](https://github.com/lydakis/sandfs/blob/main/crates/sandfs-cli/README.md) says more about how the commands behave.

## The one line in `main`

A mount is served by one background process per home, which outlives the program that asked for it. sandfs starts that process by running the current program again, with the words given to `handle_relaunch` followed by its own (`daemon --serve`); git's file monitor in a git mount and `shell` on macOS run it the same way. So `handle_relaunch` must come first in `main`: in those runs it does what they were started for and exits. A program with commands of its own gives a prefix none of them uses, such as `&["sandfs"]`, so `PROGRAM sandfs ...` is sandfs's and everything else is the program's; the `sandfs` command gives none.

The home is `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else `~/.local/share/sandfs`, shared with the `sandfs` command, or any folder with `Home::at`. Without the default `mount` feature (and on Windows) everything but mounting works.

## Examples

- [`examples/attempts.rs`](https://github.com/lydakis/sandfs/blob/main/crates/sandfs/examples/attempts.rs) makes two copies of a folder of notes, tidies each a different way, lists what each changed and writes the better one back, all in a temporary folder: `cargo run -p sandfs --example attempts` in a checkout of sandfs.
- [`examples/embed.rs`](https://github.com/lydakis/sandfs/blob/main/crates/sandfs/examples/embed.rs) is a program with commands of its own that hands the rest to sandfs.
- [Graft](https://github.com/lydakis/graft) is a real one: it gives each task its own copy of a GitHub repository and turns a finished copy into a pull request, and its users never install `sandfs`.

## Before you build on it

- **The API will change.** sandfs is pre-1.0, so a minor release (0.9 to 0.10) may change this crate's API; the [changelog](https://github.com/lydakis/sandfs/blob/main/docs/changelog.md) says how each time. Depend on one minor version, such as `sandfs = "0.9"`.
- **Writing in a mount costs more than writing to a plain folder, most of all on macOS.** Every file operation in a mount goes through the mount's process. On macOS each write is also a round trip over localhost before it returns, so write-heavy work is where a Mac feels it: compiling ripgrep from scratch took about 18 s in a mount against 13 s in a plain folder on the same Mac (October 2026).
- **Linux needs FUSE 3** (`fusermount3`, package `fuse3`) to mount.
- **sandfs isn't an execution sandbox.** A program working in a mount can still reach anything else its user can; `Home::shell` runs one confined to the mount by the operating system's own sandbox.

[`sandfs-core`](https://crates.io/crates/sandfs-core) is the in-memory engine underneath: sources, forks and changes, with nothing on disk and no mount, for a program that only needs copies in memory.

## License

MIT, except `src/nfsserve`, which is from the nfsserve crate under BSD-3-Clause (see its LICENSE).
