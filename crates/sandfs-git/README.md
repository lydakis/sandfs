# sandfs-git

An optional [sandfs](https://crates.io/crates/sandfs-core) source that mounts a git commit without checking it out. Trees and blobs are read on demand through a `git cat-file --batch` process (and a few more while several threads read at once), and in a partial clone (`git clone --filter=blob:none`) git fetches a file's contents only when it is first read. It needs the `git` command on `PATH`.

```rust,no_run
use std::sync::Arc;
use sandfs_core::Workspace;
use sandfs_git::GitSource;

fn main() -> sandfs_core::Result<()> {
    let mut ws = Workspace::new();
    ws.mount("/project", Arc::new(GitSource::open("path/to/repo", "HEAD")?))?;
    println!("{}", ws.read_text("/project/README.md")?);
    Ok(())
}
```

It is also a reference for writing a source of your own on the `Source` trait. See the [repository](https://github.com/lydakis/sandfs) for the design.

License: MIT
