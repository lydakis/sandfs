//! A program that writes a lot: `cargo build` of one of this repository's packages, in
//! a folder on the host and in a sandfs mount of the same folder. The first build
//! writes every compiled file into the folder; a rebuild after changing one file
//! rereads what the first build wrote and rewrites part of it, the way a person or an
//! agent rebuilds after each edit.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::targets::{Fuse, Target};

/// The `--only` name of this table.
pub const NAME: &str = "build";

/// Names left out of the copy: build output, version control and local tools.
const SKIPPED: &[&str] = &["target", ".git", ".venv", "node_modules"];

/// One column: where the build ran, and its times.
pub struct Column {
    pub name: &'static str,
    pub cold: Duration,
    pub rebuilds: Vec<Duration>,
    /// Resident memory of the process serving the mount once the builds are done.
    pub server_rss: Option<u64>,
}

pub struct BuildResult {
    pub package: String,
    pub columns: Vec<Column>,
}

/// Build `package` from a copy of this repository, on the host (`host`) and, when
/// asked (`mount`) and `bin` can mount here, in a mount; rebuild `iters` times after
/// changing its first source file. A mount column that was asked for by name
/// (`mount_named`) and can't run is an error, not a missing column.
pub fn run(
    package: &str,
    scratch: &Path,
    bin: &Path,
    iters: usize,
    host: bool,
    (mount, mount_named): (bool, bool),
) -> Result<BuildResult> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let changed = changed_file(&repo, package)?;
    ensure!(host || mount, "the build table runs on the host and fuse targets only");
    // A scratch folder inside the repository is left out of its copies, with the data
    // set and anything else the run put there.
    let scratch = scratch.canonicalize()?;
    let base = scratch.join("build");
    let mut columns = Vec::new();
    if host {
        let dir = base.join("host");
        copy_source(&repo, &dir, &scratch)?;
        fetch(&dir)?;
        columns.push(build_in("host folder", &dir, package, &changed, iters, None)?);
    }
    if mount {
        let mounted = base.join("mounted");
        copy_source(&repo, &mounted, &scratch)?;
        fetch(&mounted)?;
        match Fuse::new(mounted, base.clone(), bin.to_owned()) {
            Ok(fuse) => {
                let view = fuse.open(true)?;
                let root = view.root_dir().context("a mount has a folder")?.to_owned();
                columns.push(build_in("sandfs mount", &root, package, &changed, iters, Some(&fuse))?);
            }
            Err(err) if mount_named || !host => return Err(err),
            Err(err) => eprintln!("build: no mount column ({err:#})"),
        }
    }
    Ok(BuildResult { package: package.to_owned(), columns })
}

/// Fetch the dependencies once, so no build waits on the network.
fn fetch(dir: &Path) -> Result<()> {
    cargo(dir, &["fetch", "--quiet"]).context("cargo fetch (the build table needs the network once)")
}

fn build_in(
    name: &'static str,
    dir: &Path,
    package: &str,
    changed: &Path,
    iters: usize,
    fuse: Option<&Fuse>,
) -> Result<Column> {
    eprintln!("{NAME}: {package} in a {name}");
    let build = || -> Result<Duration> {
        let start = Instant::now();
        // The output goes inside the folder, whatever a cargo config says.
        let target = dir.join("target");
        let target = target.to_str().context("a non-UTF-8 folder")?;
        cargo(dir, &["build", "--offline", "--quiet", "-p", package, "--target-dir", target])?;
        Ok(start.elapsed())
    };
    let cold = build()?;
    let mut rebuilds = Vec::new();
    for round in 0..iters {
        // A real edit: a comment the compiler has to read, not just a newer timestamp.
        let mut file = fs::File::options().append(true).open(dir.join(changed))?;
        writeln!(file, "// edit {round}")?;
        drop(file);
        rebuilds.push(build()?);
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

/// Copy the repository's files, without build output, version control or `scratch`.
fn copy_source(from: &Path, to: &Path, scratch: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIPPED.iter().any(|skipped| name == *skipped) || entry.path() == scratch {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_source(&entry.path(), &to.join(&name), scratch)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to.join(&name))?;
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
    println!(
        "Building `{}` with cargo (debug, dependencies fetched), the build output inside the folder. A rebuild changes one source file first; median of {}.",
        result.package,
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
    row("first build", &|column| crate::fmt_duration(column.cold));
    row("rebuild after changing one file", &|column| crate::fmt_duration(crate::median(&column.rebuilds)));
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
    serde_json::json!({ "package": result.package, "columns": columns })
}
