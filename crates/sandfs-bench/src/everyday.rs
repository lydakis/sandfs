//! Everyday operations on a mounted folder, the way an agent or a person uses one: list
//! a folder, list it with sizes, find every file below one, look up a file deep down,
//! read one, search a folder, create, edit, rename and delete a file, and list the
//! changes.
//!
//! Each operation runs on a fresh view, so whatever sandfs caches starts empty, and on
//! every way of reaching the same data: the host's own filesystem where there is one,
//! sandfs through the Rust API (what the Python package and the MCP server call), and
//! the `sandfs` command's mount. The same operations run on three kinds of source: a
//! folder on the host, a source with a fixed delay per call (an API or an object store),
//! and a repository on a git server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use sandfs_core::{MemorySource, Workspace};
use sandfs_git::GitSource;

use crate::data::Data;
use crate::lazy::Slow;
use crate::remote::{self, Plan};
use crate::targets::{Fuse, Target, View, workspace_view};

/// The byte pattern searched for: common in text, present in random bytes now and then.
const NEEDLE: &[u8] = b"the ";

/// An everyday operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Ls,
    LsLong,
    Find,
    Stat,
    Cat,
    Grep,
    Create,
    Edit,
    Rename,
    Delete,
    Changes,
}

impl Op {
    pub const ALL: [Op; 11] = [
        Op::Ls,
        Op::LsLong,
        Op::Find,
        Op::Stat,
        Op::Cat,
        Op::Grep,
        Op::Create,
        Op::Edit,
        Op::Rename,
        Op::Delete,
        Op::Changes,
    ];

    /// The command a person would type for it.
    pub fn name(self) -> &'static str {
        match self {
            Op::Ls => "ls",
            Op::LsLong => "ls -l",
            Op::Find => "find",
            Op::Stat => "stat",
            Op::Cat => "cat",
            Op::Grep => "grep -r",
            Op::Create => "create",
            Op::Edit => "edit",
            Op::Rename => "mv",
            Op::Delete => "rm",
            Op::Changes => "changes",
        }
    }

    /// Whether the view must accept writes.
    fn writes(self) -> bool {
        matches!(self, Op::Create | Op::Edit | Op::Rename | Op::Delete | Op::Changes)
    }
}

/// The paths the operations use, relative to the view's root.
pub struct Paths {
    /// Listed by `ls` and `ls -l`.
    pub folder: String,
    /// Walked by `find` and read whole by `grep -r`; empty for the root.
    pub subtree: String,
    /// Looked up by `stat`: the deepest file there is.
    pub deep: String,
    /// Read by `cat`.
    pub read: String,
    pub edit: String,
    pub rename: String,
    pub delete: String,
    /// A file `create` adds.
    pub create: String,
}

impl Paths {
    /// What each operation does here, in words.
    fn what(&self, op: Op) -> String {
        let folder = |path: &str| if path.is_empty() { "/".to_owned() } else { format!("{path}/") };
        match op {
            Op::Ls => format!("list {}", folder(&self.folder)),
            Op::LsLong => format!("list {} with every entry's size", folder(&self.folder)),
            Op::Find => format!("list every folder below {}", folder(&self.subtree)),
            Op::Stat => format!("size of a file {} folders down", self.deep.matches('/').count()),
            Op::Cat => format!("read {}", self.read),
            Op::Grep => format!("read every file below {}", folder(&self.subtree)),
            Op::Create => format!("write 4 KiB to {}", self.create),
            Op::Edit => format!("replace 16 bytes in the middle of {}", self.edit),
            Op::Rename => format!("move {}", self.rename),
            Op::Delete => format!("delete {}", self.delete),
            Op::Changes => "list the changes after create, edit, mv and rm".to_owned(),
        }
    }

    fn renamed(&self) -> String {
        format!("{}.moved", self.rename)
    }
}

/// One kind of source and the ways of reaching it.
pub struct Group {
    /// What the source is, for the table's heading.
    pub source: String,
    pub paths: Paths,
    pub columns: Vec<Box<dyn Target>>,
    /// Timed runs of each operation.
    pub iters: usize,
}

/// Every run's time, by operation and column.
pub struct GroupResult {
    pub source: String,
    pub columns: Vec<&'static str>,
    pub iters: usize,
    pub rows: Vec<(Op, String, Vec<Vec<Duration>>)>,
}

/// Run every selected operation on every column of `group`. Each operation's result
/// (entries listed, bytes read, changes found) must be the same in every column, so a
/// view that silently does less fails instead of looking fast.
pub fn run(group: &Group, selected: &dyn Fn(&str) -> bool) -> Result<GroupResult> {
    let mut rows = Vec::new();
    for op in Op::ALL.into_iter().filter(|op| selected(&format!("everyday/{}", op.name()))) {
        eprintln!("everyday/{} ({})", op.name(), group.source);
        let mut cells = Vec::new();
        let mut expected: Option<(u64, &str)> = None;
        for column in &group.columns {
            let mut times = Vec::new();
            for _ in 0..group.iters {
                let mut view = column.open(op.writes())?;
                if op == Op::Changes {
                    for op in [Op::Create, Op::Edit, Op::Rename, Op::Delete] {
                        perform(view.as_mut(), &group.paths, op)?;
                    }
                }
                let start = Instant::now();
                let result = perform(view.as_mut(), &group.paths, op)
                    .with_context(|| format!("{} on {}", op.name(), column.name()))?;
                times.push(start.elapsed());
                drop(view);
                match expected {
                    None => expected = Some((result, column.name())),
                    Some((want, first)) => ensure!(
                        result == want,
                        "{} on {} gave {result}, but {want} on {first}",
                        op.name(),
                        column.name()
                    ),
                }
            }
            cells.push(times);
        }
        rows.push((op, group.paths.what(op), cells));
    }
    Ok(GroupResult {
        source: group.source.clone(),
        columns: group.columns.iter().map(|column| column.name()).collect(),
        iters: group.iters,
        rows,
    })
}

/// Do `op` once; returns a number every way of doing it must agree on.
fn perform(view: &mut dyn View, paths: &Paths, op: Op) -> Result<u64> {
    Ok(match op {
        Op::Ls => view.list(&paths.folder)?.len() as u64,
        Op::LsLong => {
            let mut total = 0;
            for (name, is_dir) in view.list(&paths.folder)? {
                let size = view.stat(&join(&paths.folder, &name))?;
                total += if is_dir { 1 } else { size };
            }
            total
        }
        Op::Find => find(view, &paths.subtree)?.len() as u64,
        Op::Stat => view.stat(&paths.deep)?,
        Op::Cat => view.read(&paths.read)? as u64,
        Op::Grep => {
            let mut matches = 0;
            for path in find(view, &paths.subtree)? {
                let data = view.contents(&path)?;
                matches += u64::from(data.windows(NEEDLE.len()).any(|window| window == NEEDLE));
            }
            matches
        }
        Op::Create => {
            view.write(&paths.create, &[b'n'; 4096])?;
            4096
        }
        Op::Edit => {
            let mut data = view.contents(&paths.edit)?;
            let middle = data.len() / 2;
            let end = (middle + 16).min(data.len());
            data[middle..end].fill(b'e');
            view.write(&paths.edit, &data)?;
            data.len() as u64
        }
        Op::Rename => {
            view.rename(&paths.rename, &paths.renamed())?;
            1
        }
        Op::Delete => {
            view.remove(&paths.delete)?;
            1
        }
        Op::Changes => view.changes()? as u64,
    })
}

/// Every file below `dir`, found by listing each folder once.
fn find(view: &dyn View, dir: &str) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        for (name, is_dir) in view.list(&dir)? {
            let path = join(&dir, &name);
            if is_dir {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

// ----------------------------------------------------------------------
// Sources and the ways of reaching them
// ----------------------------------------------------------------------

/// The generated data set's paths for these operations.
pub fn data_paths(data: &Data, files: usize) -> Paths {
    let small = &data.small[..files.min(data.small.len())];
    Paths {
        folder: data.small_dirs[1].clone(),
        subtree: "small".to_owned(),
        deep: data.deep_file.clone(),
        read: small[small.len() - 1].clone(),
        edit: small[1].clone(),
        rename: small[2].clone(),
        delete: small[3].clone(),
        create: format!("{}/new.bin", data.small_dirs[1]),
    }
}

/// sandfs through the Rust API over a source with a fixed delay per call, holding the
/// first `files` small files and the deep file.
pub struct SlowSource {
    files: Vec<(String, Bytes)>,
    latency: Duration,
}

impl SlowSource {
    pub fn new(data: &Data, files: usize, latency: Duration) -> Result<SlowSource> {
        let mut paths: Vec<&String> = data.small.iter().take(files).collect();
        paths.push(&data.deep_file);
        let files = paths
            .into_iter()
            .map(|path| Ok((path.clone(), Bytes::from(std::fs::read(data.root().join(path))?))))
            .collect::<Result<_>>()?;
        Ok(SlowSource { files, latency })
    }
}

impl Target for SlowSource {
    fn name(&self) -> &'static str {
        "sandfs"
    }

    fn open(&self, _writable: bool) -> Result<Box<dyn View>> {
        let inner = MemorySource::from_files(self.files.iter().map(|(path, data)| (path.as_str(), data.clone())))?;
        let mut ws = Workspace::new();
        ws.mount("/data", Arc::new(Slow::new(inner, self.latency)))?;
        Ok(workspace_view(ws, None))
    }
}

/// The remote repository's paths for these operations: the searched folder is listed,
/// walked and searched, and the sampled files are read and changed.
pub fn remote_paths(plan: &Plan) -> Paths {
    Paths {
        folder: plan.subtree.clone(),
        subtree: plan.subtree.clone(),
        deep: plan.deepest.clone(),
        read: plan.samples[0].clone(),
        edit: plan.samples[1].clone(),
        rename: plan.samples[2].clone(),
        delete: plan.samples[3].clone(),
        create: join(&plan.subtree, "sandfs-bench-new.txt"),
    }
}

/// sandfs through the Rust API over a [`GitSource`], each view on a new blobless copy
/// of the commit that has fetched no file yet.
pub struct RemoteLibrary {
    pub url: String,
    pub commit: String,
    pub scratch: PathBuf,
}

impl Target for RemoteLibrary {
    fn name(&self) -> &'static str {
        "sandfs"
    }

    fn open(&self, _writable: bool) -> Result<Box<dyn View>> {
        let dir = tempfile::Builder::new().prefix("remote-library-").tempdir_in(&self.scratch)?;
        remote::blobless(&self.url, &self.commit, dir.path())?;
        let mut ws = Workspace::new();
        ws.mount("/data", Arc::new(GitSource::open(dir.path(), &self.commit)?))?;
        Ok(workspace_view(ws, Some(dir)))
    }
}

/// The `sandfs` command's mount of the commit, each view with a `SANDFS_HOME` of its
/// own so it starts with nothing fetched.
pub struct RemoteMount {
    pub url: String,
    pub commit: String,
    pub scratch: PathBuf,
    pub bin: PathBuf,
}

impl Target for RemoteMount {
    fn name(&self) -> &'static str {
        "sandfs mount"
    }

    fn open(&self, writable: bool) -> Result<Box<dyn View>> {
        let source = PathBuf::from(format!("git:{}@{}", self.url, self.commit));
        Fuse::new(source, self.scratch.clone(), self.bin.clone())?.open(writable)
    }
}

/// Whether `bin` can mount here, for leaving the mount columns out where it can't.
pub fn can_mount(scratch: &Path, bin: &Path) -> Result<()> {
    let probe = tempfile::Builder::new().prefix("everyday-probe-").tempdir_in(scratch)?;
    Fuse::new(probe.path().to_owned(), scratch.to_owned(), bin.to_owned()).map(drop)
}

// ----------------------------------------------------------------------
// Report
// ----------------------------------------------------------------------

pub fn print(result: &GroupResult) {
    println!();
    println!(
        "Everyday operations on {}: median of {} run{}, each on a fresh view.",
        result.source,
        result.iters,
        if result.iters == 1 { "" } else { "s" }
    );
    println!();
    println!("| operation | what | {} |", result.columns.join(" | "));
    println!("|---|---|{}", "---:|".repeat(result.columns.len()));
    for (op, what, cells) in &result.rows {
        let cells: Vec<String> = cells.iter().map(|times| crate::fmt_duration(crate::median(times))).collect();
        println!("| {} | {what} | {} |", op.name(), cells.join(" | "));
    }
}

pub fn json(result: &GroupResult) -> serde_json::Value {
    let rows: Vec<_> = result
        .rows
        .iter()
        .map(|(op, what, cells)| {
            let cells: serde_json::Map<_, _> = result
                .columns
                .iter()
                .zip(cells)
                .map(|(column, times)| ((*column).to_owned(), serde_json::json!(crate::nanos(times))))
                .collect();
            serde_json::json!({ "operation": op.name(), "what": what, "nanos": cells })
        })
        .collect();
    serde_json::json!({ "source": result.source, "rows": rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Data, Scale};
    use crate::targets::{Host, Sandfs};

    #[test]
    fn every_operation_agrees_between_the_host_and_sandfs() {
        let scratch = tempfile::tempdir().unwrap();
        let data = Data::generate(Scale::TINY, scratch.path()).unwrap();
        let group = Group {
            source: "a test folder".to_owned(),
            paths: data_paths(&data, usize::MAX),
            columns: vec![
                Box::new(Host { data: data.root().to_owned(), scratch: scratch.path().to_owned() }),
                Box::new(Sandfs { data: data.root().to_owned() }),
                Box::new(SlowSource::new(&data, usize::MAX, Duration::ZERO).unwrap()),
            ],
            iters: 1,
        };
        let result = run(&group, &|_| true).unwrap();
        assert_eq!(result.rows.len(), Op::ALL.len());
    }
}
