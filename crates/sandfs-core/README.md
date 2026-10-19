# sandfs-core

A lazy, copy-on-write sandbox filesystem for agents. A `Workspace` mounts read-only sources (a host folder, in-memory files, or any tree you implement with `read_dir` and `read_file`), keeps every write in an in-memory overlay, forks in O(1), and reports which files changed. Files are bytes of any kind, and sources are never modified. A host folder is read when each path is first touched, not copied, so mount one nothing else is changing.

```rust,no_run
use std::sync::Arc;
use sandfs_core::{DirSource, MemorySource, Workspace};

fn main() -> sandfs_core::Result<()> {
    let mut base = Workspace::new();
    base.mount("/reports", Arc::new(DirSource::new("./reports")?))?;
    base.mount("/brief", Arc::new(MemorySource::from_files([("task.md", "Summarise open tickets\n")])?))?;

    let mut attempt = base.fork(); // O(1); `base` never sees its changes
    attempt.write("/reports/summary.md", "# Open tickets\n")?;
    for change in attempt.changes()? {
        println!("{:?} {}", change.kind, change.path); // Added /reports/summary.md
    }
    Ok(())
}
```

sandfs does not run commands; point a container or microVM at the files for that. Python bindings are published as [`sandfs`](https://pypi.org/project/sandfs/), and a lazy git source is the optional [`sandfs-git`](https://crates.io/crates/sandfs-git) crate. See the [repository](https://github.com/lydakis/sandfs) for the design and roadmap.

License: MIT
