//! The workloads, each a timed operation on a fresh view plus optional untimed setup.

use anyhow::{Result, ensure};

use crate::data::{Data, Rng};
use crate::targets::View;

/// Whatever a timed run produced that must outlive the timer, such as a fork, so that
/// tearing it down (an unmount) is not counted.
pub type Leftover = Option<Box<dyn View>>;

type Step = Box<dyn Fn(&Data, &mut dyn View) -> Result<Leftover>>;

/// One benchmark: `prepare` runs untimed on a fresh view, then `run` is timed, then
/// `check` confirms untimed that the run did all its work, on the fork when `run`
/// returns one.
pub struct Workload {
    /// Short name, `group/name`.
    pub name: &'static str,
    /// What one timed run does, in words.
    pub what: String,
    /// Whether the view must accept writes.
    pub writable: bool,
    pub prepare: Step,
    pub run: Step,
    pub check: Step,
}

fn step(f: impl Fn(&Data, &mut dyn View) -> Result<()> + 'static) -> Step {
    Box::new(move |data, view| f(data, view).map(|()| None))
}

/// A check that the view reports exactly `expected` changed files.
fn changed(expected: usize) -> Step {
    step(move |_, view| {
        let found = view.changes()?;
        ensure!(found == expected, "found {found} changes, expected {expected}");
        Ok(())
    })
}

fn nothing() -> Step {
    step(|_, _| Ok(()))
}

/// The bytes every write workload writes: 4 KiB that differ from any original file.
fn payload() -> Vec<u8> {
    Rng(0xd1ff).bytes(4096)
}

fn read_all(data: &Data, view: &mut dyn View) -> Result<()> {
    let mut total = 0u64;
    for path in &data.small {
        total += view.read(path)? as u64;
    }
    ensure!(total == data.small_bytes, "read {total} bytes, expected {}", data.small_bytes);
    Ok(())
}

fn stat_all(data: &Data, view: &mut dyn View) -> Result<()> {
    let mut total = 0;
    for path in &data.small {
        total += view.stat(path)?;
    }
    ensure!(total == data.small_bytes, "stat summed to {total} bytes, expected {}", data.small_bytes);
    Ok(())
}

fn walk(view: &mut dyn View, dir: &str) -> Result<usize> {
    let mut files = 0;
    for (name, is_dir) in view.list(dir)? {
        let path = format!("{dir}/{name}");
        if is_dir { files += walk(view, &path)? } else { files += 1 }
    }
    Ok(files)
}

fn create(data: &Data, view: &mut dyn View) -> Result<()> {
    let payload = payload();
    for i in 0..data.scale.ops {
        let dir = &data.small_dirs[1 + i % (data.small_dirs.len() - 1)];
        view.write(&format!("{dir}/new{i:05}.bin"), &payload)?;
    }
    Ok(())
}

fn overwrite(data: &Data, view: &mut dyn View) -> Result<()> {
    let payload = payload();
    for path in data.spread(data.scale.ops) {
        view.write(path, &payload)?;
    }
    Ok(())
}

fn check_created(data: &Data, view: &mut dyn View) -> Result<()> {
    for i in 0..data.scale.ops {
        let dir = &data.small_dirs[1 + i % (data.small_dirs.len() - 1)];
        let size = view.stat(&format!("{dir}/new{i:05}.bin"))?;
        ensure!(size == 4096, "new file {i} has {size} bytes");
    }
    Ok(())
}

fn check_overwritten(data: &Data, view: &mut dyn View) -> Result<()> {
    for path in data.spread(data.scale.ops) {
        let size = view.read(path)?;
        ensure!(size == 4096, "{path} has {size} bytes after the overwrite");
    }
    Ok(())
}

/// Files to delete: offset by one from the overwritten ones, so the sets never meet.
fn doomed(data: &Data, n: usize) -> Vec<&str> {
    let step = (data.small.len() / data.scale.ops.max(1)).max(2);
    data.small.iter().skip(1).step_by(step).take(n).map(String::as_str).collect()
}

/// Every workload, in the order they are reported.
pub fn all(data: &Data) -> Vec<Workload> {
    let s = data.scale;
    let files = s.small_files();
    let mib = s.large_mib;
    let ops = s.ops;
    let deep_stats = ops * 10;
    let deletes = (ops / 10).max(1);
    vec![
        Workload {
            name: "small/read-cold",
            what: format!("read {files} small files once, on a fresh view"),
            writable: false,
            prepare: nothing(),
            run: step(read_all),
            check: nothing(),
        },
        Workload {
            name: "small/read-warm",
            what: format!("read the {files} files again"),
            writable: false,
            prepare: step(read_all),
            run: step(read_all),
            check: nothing(),
        },
        Workload {
            name: "small/stat",
            what: format!("stat the {files} files, after listing their directories"),
            writable: false,
            prepare: step(|_, view| walk(view, "small").map(drop)),
            run: step(stat_all),
            check: nothing(),
        },
        Workload {
            name: "small/walk",
            what: format!("list every directory of the {files}-file tree, on a fresh view"),
            writable: false,
            prepare: nothing(),
            run: step(move |_, view| {
                let found = walk(view, "small")?;
                ensure!(found == files, "walk found {found} files, expected {files}");
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "small/walk-warm",
            what: format!("list every directory of the {files}-file tree again"),
            writable: false,
            prepare: step(|_, view| walk(view, "small").map(drop)),
            run: step(move |_, view| {
                let found = walk(view, "small")?;
                ensure!(found == files, "walk found {found} files, expected {files}");
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "wide/list",
            what: format!("list one directory of {} files (already read once)", s.wide),
            writable: false,
            prepare: step(|_, view| view.list("wide").map(drop)),
            run: step(move |data, view| {
                let found = view.list("wide")?.len();
                ensure!(found == data.scale.wide, "listed {found} entries");
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "deep/stat",
            what: format!("stat a file {} directories deep, {deep_stats} times", s.depth),
            writable: false,
            prepare: step(|data, view| view.stat(&data.deep_file).map(drop)),
            run: step(move |data, view| {
                for _ in 0..deep_stats {
                    view.stat(&data.deep_file)?;
                }
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "large/read-whole",
            what: format!("read a {mib} MiB file whole, on a fresh view"),
            writable: false,
            prepare: nothing(),
            run: step(|data, view| {
                let read = view.read(&data.large_file)? as u64;
                ensure!(read == data.large_size, "read {read} bytes");
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "large/read-ranges",
            what: format!("read {ops} random 64 KiB ranges of the {mib} MiB file, on a fresh view"),
            writable: false,
            prepare: nothing(),
            run: step(|data, view| {
                let mut rng = Rng(42);
                let len = 64 << 10;
                for _ in 0..data.scale.ops {
                    let offset = rng.next() % (data.large_size - len);
                    let read = view.read_range(&data.large_file, offset, len)?;
                    ensure!(read as u64 == len, "short read of {read} bytes");
                }
                Ok(())
            }),
            check: nothing(),
        },
        Workload {
            name: "write/create",
            what: format!("create {ops} new 4 KiB files"),
            writable: true,
            prepare: nothing(),
            run: step(create),
            check: step(check_created),
        },
        Workload {
            name: "write/overwrite",
            what: format!("overwrite {ops} existing files with 4 KiB each"),
            writable: true,
            prepare: nothing(),
            run: step(overwrite),
            check: step(check_overwritten),
        },
        Workload {
            name: "write/delete",
            what: format!("delete {ops} existing files"),
            writable: true,
            prepare: nothing(),
            run: step(|data, view| {
                for path in doomed(data, data.scale.ops) {
                    view.remove(path)?;
                }
                Ok(())
            }),
            check: step(|data, view| {
                for path in doomed(data, data.scale.ops) {
                    ensure!(view.stat(path).is_err(), "{path} still exists");
                }
                Ok(())
            }),
        },
        Workload {
            name: "fork/clean",
            what: format!("fork a view of the {files}-file tree with no changes"),
            writable: true,
            prepare: nothing(),
            run: Box::new(|_, view| view.fork().map(Some)),
            check: changed(0),
        },
        Workload {
            name: "fork/changed",
            what: format!("fork a view with {} changed files", ops * 2),
            writable: true,
            prepare: step(|data, view| {
                create(data, view)?;
                overwrite(data, view)
            }),
            run: Box::new(|_, view| view.fork().map(Some)),
            check: changed(ops * 2),
        },
        Workload {
            name: "changes",
            what: format!("list the changes after {ops} creates, {ops} overwrites and {deletes} deletes"),
            writable: true,
            prepare: step(move |data, view| {
                create(data, view)?;
                overwrite(data, view)?;
                for path in doomed(data, deletes) {
                    view.remove(path)?;
                }
                Ok(())
            }),
            run: step(move |_, view| {
                let found = view.changes()?;
                let expected = ops * 2 + deletes;
                ensure!(found == expected, "found {found} changes, expected {expected}");
                Ok(())
            }),
            check: nothing(),
        },
    ]
}
