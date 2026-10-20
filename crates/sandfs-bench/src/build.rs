//! A program that reads and writes a lot, in a folder on the host and in a sandfs mount
//! of the same folder: by default `cargo build` of one of this repository's packages,
//! or any command in any folder (a test suite, another language's build). The first
//! run writes every compiled file into the folder; a run after changing one file
//! rereads what the first wrote and rewrites part of it, the way a person or an agent
//! rebuilds after each edit.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::targets::{Fuse, Target};

/// The `--only` name of this table.
pub const NAME: &str = "build";

/// Names left out of a copy of this repository: build output, version control and
/// local tools. A folder given with `--build-dir` is copied whole but for `.git`.
const SKIPPED: &[&str] = &["target", ".git", ".venv", "node_modules"];

/// What to run, and where.
pub enum Job {
    /// `cargo build -p PACKAGE` in a copy of this repository.
    Cargo { package: String },
    /// `sh -c COMMAND` in a copy of `dir`, changing `change` (a path inside it) before
    /// each run after the first when there is one.
    Command { dir: PathBuf, command: String, change: Option<PathBuf> },
}

impl Job {
    fn describe(&self) -> String {
        match self {
            Job::Cargo { package } => format!(
                "Building `{package}` with cargo (debug, dependencies fetched), the build output inside the folder. Each run after the first changes one source file"
            ),
            Job::Command { dir, command, change } => format!(
                "Running `{command}` in a copy of {}. {}",
                dir.display(),
                match change {
                    Some(change) => format!("Each run after the first changes {}", change.display()),
                    None => "Nothing changes between runs".to_owned(),
                }
            ),
        }
    }
}

/// One column: where the build ran, and its times.
pub struct Column {
    pub name: &'static str,
    pub cold: Duration,
    pub rebuilds: Vec<Duration>,
    /// Resident memory of the process serving the mount once the builds are done.
    pub server_rss: Option<u64>,
}

pub struct BuildResult {
    pub job: String,
    /// How many copies or forks ran the job at once.
    pub parallel: usize,
    pub columns: Vec<Column>,
}

/// Run `job` in a copy of its folder on the host (`host`) and, when asked (`mount`) and
/// `bin` can mount here, in a mount of another copy; run it again `iters` times. With
/// `parallel` above one, the job runs that many times at once: in as many copies on the
/// host, and in as many forks of one mount. A mount column that was asked for by name
/// (`mount_named`) and can't run is an error, not a missing column.
#[allow(clippy::too_many_arguments)]
pub fn run(
    job: &Job,
    scratch: &Path,
    bin: &Path,
    iters: usize,
    parallel: usize,
    host: bool,
    (mount, mount_named): (bool, bool),
) -> Result<BuildResult> {
    let (from, skipped, change): (PathBuf, &[&str], Option<PathBuf>) = match job {
        Job::Cargo { package } => {
            let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
            let change = changed_file(&repo, package)?;
            (repo, SKIPPED, Some(change))
        }
        Job::Command { dir, change, .. } => {
            let dir = dir.canonicalize().with_context(|| format!("--build-dir {}", dir.display()))?;
            if let Some(change) = change {
                // The runs change copies of this file, so it must be one inside the
                // folder: not an absolute path, and not a link out of it.
                let file = dir.join(change).canonicalize().ok();
                ensure!(
                    change.components().all(|part| matches!(part, Component::Normal(_)))
                        && file.is_some_and(|file| file.starts_with(&dir) && file.is_file()),
                    "--build-change {} is no file in {}",
                    change.display(),
                    dir.display()
                );
            }
            (dir, &[".git"], change.clone())
        }
    };
    ensure!(host || mount, "the build table runs on the host and fuse targets only");
    ensure!(parallel > 0, "--build-parallel must be at least 1");
    // A scratch folder inside the copied folder is left out of its copies, with the data
    // set and anything else the run put there.
    let scratch = scratch.canonicalize()?;
    let base = scratch.join("build");
    let copy = |name: &str| -> Result<PathBuf> {
        let dir = base.join(name);
        copy_source(&from, &dir, &scratch, skipped)?;
        if matches!(job, Job::Cargo { .. }) {
            fetch(&dir)?;
        }
        Ok(dir)
    };
    // Each column has its own empty cache folder, so a tool that caches there (Go's
    // build cache, pip) can't reuse what the other column computed.
    let cache = |name: &str| -> Result<PathBuf> {
        let dir = base.join(format!("cache-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    };
    let mut columns = Vec::new();
    if host {
        let (name, dirs) = match parallel {
            1 => ("host folder", vec![copy("host")?]),
            _ => ("host folders", (0..parallel).map(|i| copy(&format!("host-{i}"))).collect::<Result<_>>()?),
        };
        columns.push(run_in(name, &dirs, &cache("host")?, job, change.as_deref(), iters, None)?);
    }
    if mount {
        match Fuse::new(copy("mounted")?, base.clone(), bin.to_owned()) {
            Ok(fuse) => {
                let mut view = fuse.open(true)?;
                // Forks of one mount, as attempts branching from one prepared folder are.
                let (name, views) = match parallel {
                    1 => ("sandfs mount", vec![view]),
                    _ => ("sandfs forks", (0..parallel).map(|_| view.fork()).collect::<Result<_>>()?),
                };
                let dirs: Vec<PathBuf> = views
                    .iter()
                    .map(|view| view.root_dir().context("a mount has a folder").map(Path::to_owned))
                    .collect::<Result<_>>()?;
                let cache = cache("mounted")?;
                columns.push(run_in(name, &dirs, &cache, job, change.as_deref(), iters, Some(&fuse))?);
            }
            Err(err) if mount_named || !host => return Err(err),
            Err(err) => eprintln!("build: no mount column ({err:#})"),
        }
    }
    Ok(BuildResult { job: job.describe(), parallel, columns })
}

/// Fetch the dependencies once, so no build waits on the network.
fn fetch(dir: &Path) -> Result<()> {
    cargo(dir, &["fetch", "--quiet"]).context("cargo fetch (the build table needs the network once)")
}

/// Run `job` in every one of `dirs` at once, timing until the last one finishes.
fn run_in(
    name: &'static str,
    dirs: &[PathBuf],
    cache: &Path,
    job: &Job,
    change: Option<&Path>,
    iters: usize,
    fuse: Option<&Fuse>,
) -> Result<Column> {
    eprintln!("{NAME}: in {} {name}", dirs.len());
    let run_one = |dir: &Path| -> Result<()> {
        match job {
            Job::Cargo { package } => {
                // The output goes inside the folder, whatever a cargo config says.
                let target = dir.join("target");
                let target = target.to_str().context("a non-UTF-8 folder")?;
                cargo(dir, &["build", "--offline", "--quiet", "-p", package, "--target-dir", target])
            }
            Job::Command { command, .. } => {
                let status =
                    Command::new("sh").args(["-c", command]).current_dir(dir).env("XDG_CACHE_HOME", cache).status()?;
                ensure!(status.success(), "`{command}` failed in {}", dir.display());
                Ok(())
            }
        }
    };
    let once = || -> Result<Duration> {
        let start = Instant::now();
        std::thread::scope(|scope| {
            let runs: Vec<_> = dirs.iter().map(|dir| scope.spawn(move || run_one(dir))).collect();
            runs.into_iter().try_for_each(|run| run.join().expect("a run panicked"))
        })?;
        Ok(start.elapsed())
    };
    let cold = once()?;
    let mut rebuilds = Vec::new();
    for round in 0..iters {
        // A real edit, not just a newer timestamp: a comment the compiler has to read,
        // or for any other file blank lines, which no language minds. Every copy and
        // round gets different bytes, and the mount's lines hold a space, so no run
        // finds another's result in a cache they share.
        if let Some(change) = change {
            let blank = if fuse.is_some() { " \n" } else { "\n" };
            for (i, dir) in dirs.iter().enumerate() {
                let mut file = fs::File::options().append(true).open(dir.join(change))?;
                match job {
                    Job::Cargo { .. } => writeln!(file, "// edit {round} in copy {i} of a {name}")?,
                    // Copy i holds i + 1 + round * dirs.len() lines more than at first.
                    Job::Command { .. } => {
                        let lines = if round == 0 { i + 1 } else { dirs.len() };
                        file.write_all(blank.repeat(lines).as_bytes())?;
                    }
                }
            }
        }
        rebuilds.push(once()?);
    }
    let server_rss = fuse.and_then(|fuse| fuse.helper_pids().first().copied()).and_then(|pid| current_rss(pid).ok());
    Ok(Column { name, cold, rebuilds, server_rss })
}

/// Run cargo in `dir` with its own defaults, whatever this process was run with.
fn cargo(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("RUSTFLAGS")
        // No compiler cache: the second column would reuse what the first one built.
        // An empty value also overrides a wrapper set in a cargo config.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .status()
        .context("running cargo")?;
    ensure!(status.success(), "cargo {} failed in {}", args.join(" "), dir.display());
    Ok(())
}

/// The file a rebuild changes: the package's `src/main.rs` or `src/lib.rs`.
fn changed_file(repo: &Path, package: &str) -> Result<PathBuf> {
    let dir = Path::new("crates").join(package).join("src");
    for name in ["main.rs", "lib.rs"] {
        if repo.join(&dir).join(name).is_file() {
            return Ok(dir.join(name));
        }
    }
    bail!("no package {package} in crates/ with a src/main.rs or src/lib.rs")
}

/// Copy a folder's files and symbolic links, without `skipped` names or `scratch`.
fn copy_source(from: &Path, to: &Path, scratch: &Path, skipped: &[&str]) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skipped.iter().any(|skipped| name == *skipped) || entry.path() == scratch {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_source(&entry.path(), &to.join(&name), scratch, skipped)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to.join(&name))?;
            // Keep the time too: tools such as `make` compare times to decide what is
            // out of date.
            // Opened for reading only: setting a time needs ownership, not write access,
            // and a read-only file keeps its mode in the copy.
            fs::File::open(to.join(&name))?.set_modified(entry.metadata()?.modified()?)?;
        } else if kind.is_symlink() {
            // Such as the commands in node_modules/.bin.
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, to.join(&name))?;
        }
    }
    Ok(())
}

/// A process's current resident memory in bytes, from `VmRSS`.
fn current_rss(pid: u32) -> Result<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .context("no VmRSS in /proc status")?;
    Ok(kib * 1024)
}

pub fn print(result: &BuildResult) {
    println!();
    let at_once = match result.parallel {
        1 => String::new(),
        n => format!(", {n} at once (in copies of the folder on the host, in forks of one mount)"),
    };
    println!(
        "{}{at_once}; median of {}.",
        result.job,
        result.columns.first().map_or(0, |column| column.rebuilds.len())
    );
    println!();
    let names: Vec<&str> = result.columns.iter().map(|column| column.name).collect();
    println!("| | {} |", names.join(" | "));
    println!("|---|{}", "---:|".repeat(names.len()));
    let row = |label: &str, cell: &dyn Fn(&Column) -> String| {
        let cells: Vec<String> = result.columns.iter().map(cell).collect();
        println!("| {label} | {} |", cells.join(" | "));
    };
    row("first run", &|column| crate::fmt_duration(column.cold));
    row("run again", &|column| crate::fmt_duration(crate::median(&column.rebuilds)));
    if result.columns.iter().any(|column| column.server_rss.is_some()) {
        row("memory of the mount's server after", &|column| {
            column.server_rss.map_or_else(|| "n/a".to_owned(), crate::fmt_bytes)
        });
    }
}

pub fn json(result: &BuildResult) -> serde_json::Value {
    let columns: Vec<_> = result
        .columns
        .iter()
        .map(|column| {
            serde_json::json!({
                "column": column.name,
                "cold_nanos": column.cold.as_nanos() as u64,
                "rebuild_nanos": crate::nanos(&column.rebuilds),
                "server_rss": column.server_rss,
            })
        })
        .collect();
    serde_json::json!({ "job": result.job, "parallel": result.parallel, "columns": columns })
}
