//! Copies of a git clone already on this machine, one per task, the way an agent
//! harness gives each attempt its own folder: a git worktree, a copy-on-write copy of
//! the clone (APFS clones on macOS, reflinks where Linux has them, else a full copy),
//! and a `sandfs fork` of the clone. Each is made, then used the way an agent uses it:
//! small files written, rewritten, saved the way editors save and deleted, git status,
//! a search of the whole tree, and a build (first, unchanged, after an edit).
//!
//! The clone is a local clone of this repository, and the build is `cargo build` of
//! one of its packages with the output inside the copy.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

/// The `--only` name of this table.
pub const NAME: &str = "copies";

/// Small files each file step writes, rewrites or deletes.
const FILES: usize = 1000;

/// A way to give a task its own copy of the clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Worktree,
    Cloned,
    Fork,
}

impl Method {
    pub const ALL: [Method; 3] = [Method::Worktree, Method::Cloned, Method::Fork];

    pub fn name(self) -> &'static str {
        match self {
            Method::Worktree => "worktree",
            Method::Cloned => "cloned copy",
            Method::Fork => "sandfs fork",
        }
    }
}

/// The steps, in the order they run on each copy.
pub const STEPS: [&str; 11] = [
    "make a copy",
    "create 1,000 files",
    "rewrite them",
    "save them as editors do",
    "delete them",
    "git status",
    "search the tree",
    "first build",
    "build, nothing changed",
    "build after an edit",
    "remove the copy",
];

pub struct CopiesResult {
    pub package: String,
    pub methods: Vec<(Method, Vec<Vec<Duration>>)>,
}

/// Make copies of a clone of this repository each `method` way, `iters` of them, and
/// time each step on them.
pub fn run(scratch: &Path, bin: &Path, package: &str, iters: usize, methods: &[Method]) -> Result<CopiesResult> {
    let scratch = scratch.canonicalize()?.join(NAME);
    // Left by an earlier run; each run starts from a fresh clone.
    if scratch.exists() {
        fs::remove_dir_all(&scratch)?;
    }
    fs::create_dir_all(&scratch)?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let src = scratch.join("clone");
    run_cmd(Command::new("git").args(["clone", "-q"]).arg(&repo).arg(&src))?;
    cargo(&src, &["fetch", "--quiet"]).context("cargo fetch (the copies table needs the network once)")?;
    let change = Path::new("crates").join(package).join("src/lib.rs");
    ensure!(src.join(&change).is_file(), "no {}", change.display());
    let home = scratch.join("home");
    let mut out = Vec::new();
    for &method in methods {
        eprintln!("{NAME}: {}", method.name());
        let mut steps = vec![Vec::new(); STEPS.len()];
        for i in 0..iters {
            let dir = scratch.join(format!("{}-{i}", method.name().replace(' ', "-")));
            let branch = format!("copy-{}-{i}", method.name().replace(' ', "-"));
            steps[0].push(time(|| make(method, &src, &dir, &branch, bin, &home))?);
            // Every copy gets the file and git steps; the build runs in the first only,
            // its later rounds timed again in the same copy.
            let files = dir.join("bench-files");
            fs::create_dir_all(&files)?;
            steps[1].push(time(|| each(&files, |path, i| fs::write(path, format!("new {i}\n"))))?);
            steps[2].push(time(|| each(&files, |path, i| fs::write(path, format!("again {i}\n"))))?);
            steps[3].push(time(|| {
                each(&files, |path, i| {
                    let temp = path.with_extension("tmp");
                    fs::write(&temp, format!("saved {i}\n"))?;
                    fs::rename(&temp, path)
                })
            })?);
            steps[4].push(time(|| each(&files, |path, _| fs::remove_file(path)))?);
            fs::remove_dir(&files)?;
            run_cmd(Command::new("git").args(["status", "--porcelain"]).current_dir(&dir))?;
            steps[5].push(time(|| run_cmd(Command::new("git").args(["status", "--porcelain"]).current_dir(&dir)))?);
            steps[6].push(time(|| {
                // grep exits 1 when nothing matches, which this pattern always does.
                let status = Command::new("grep")
                    .args(["-rl", "--exclude-dir=.git", "--exclude-dir=target", "fn main"])
                    .arg(".")
                    .current_dir(&dir)
                    .stdout(std::process::Stdio::null())
                    .status()?;
                ensure!(status.code() == Some(0), "grep failed in {}", dir.display());
                Ok(())
            })?);
            if i == 0 {
                let build = || {
                    let target = dir.join("target");
                    let target = target.to_str().context("a non-UTF-8 folder")?.to_owned();
                    cargo(&dir, &["build", "--offline", "--quiet", "-p", package, "--target-dir", &target])
                };
                steps[7].push(time(build)?);
                for round in 0..iters {
                    steps[8].push(time(build)?);
                    let mut file = fs::File::options().append(true).open(dir.join(&change))?;
                    writeln!(file, "// edit {round}")?;
                    drop(file);
                    steps[9].push(time(build)?);
                }
            }
        }
        for i in 0..iters {
            let dir = scratch.join(format!("{}-{i}", method.name().replace(' ', "-")));
            steps[10].push(time(|| remove(method, &src, &dir, bin, &home))?);
        }
        out.push((method, steps));
    }
    fs::remove_dir_all(&scratch)?;
    Ok(CopiesResult { package: package.to_owned(), methods: out })
}

/// Make `dir` a copy of the clone `src`, on a new branch `branch`.
fn make(method: Method, src: &Path, dir: &Path, branch: &str, bin: &Path, home: &Path) -> Result<()> {
    match method {
        Method::Worktree => {
            run_cmd(Command::new("git").args(["worktree", "add", "-q", "-b", branch]).arg(dir).current_dir(src))
        }
        Method::Cloned => {
            let mut cp = Command::new("cp");
            if cfg!(target_os = "macos") {
                cp.args(["-c", "-R"]);
            } else {
                cp.args(["-a", "--reflink=auto"]);
            }
            run_cmd(cp.arg(src).arg(dir))?;
            run_cmd(Command::new("git").args(["checkout", "-q", "-b", branch]).current_dir(dir))
        }
        Method::Fork => {
            run_cmd(Command::new(bin).arg("fork").arg(src).arg(dir).args(["--branch", branch]).env("SANDFS_HOME", home))
        }
    }
}

fn remove(method: Method, src: &Path, dir: &Path, bin: &Path, home: &Path) -> Result<()> {
    match method {
        Method::Worktree => {
            run_cmd(Command::new("git").args(["worktree", "remove", "--force"]).arg(dir).current_dir(src))
        }
        Method::Cloned => Ok(fs::remove_dir_all(dir)?),
        Method::Fork => {
            let name = dir.file_name().context("a copy has a name")?;
            run_cmd(Command::new(bin).arg("unmount").arg(dir).env("SANDFS_HOME", home))?;
            run_cmd(Command::new(bin).arg("rm").arg(name).env("SANDFS_HOME", home))?;
            Ok(fs::remove_dir(dir)?)
        }
    }
}

/// Do `step` to each of the [`FILES`] small files in `dir`.
fn each(dir: &Path, step: impl Fn(&Path, usize) -> std::io::Result<()>) -> Result<()> {
    for i in 0..FILES {
        step(&dir.join(format!("file-{i}.txt")), i)?;
    }
    Ok(())
}

fn time(step: impl FnOnce() -> Result<()>) -> Result<Duration> {
    let start = Instant::now();
    step()?;
    Ok(start.elapsed())
}

fn run_cmd(command: &mut Command) -> Result<()> {
    let output = command.output().with_context(|| format!("running {command:?}"))?;
    ensure!(output.status.success(), "{command:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

/// Run cargo in `dir` with its own defaults, whatever this process was run with.
fn cargo(dir: &Path, args: &[&str]) -> Result<()> {
    run_cmd(
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(args)
            .current_dir(dir)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_INCREMENTAL")
            .env_remove("RUSTFLAGS")
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", ""),
    )
}

pub fn print(result: &CopiesResult) {
    println!();
    println!(
        "Copies of a clone of this repository, one per task. Builds are `cargo build -p {}`; median, and how many times a worktree's.",
        result.package
    );
    println!();
    let names: Vec<&str> = result.methods.iter().map(|(method, _)| method.name()).collect();
    println!("| | {} |", names.join(" | "));
    println!("|---|{}", "---:|".repeat(names.len()));
    let worktree = result.methods.iter().find(|(method, _)| *method == Method::Worktree);
    for (row, step) in STEPS.iter().enumerate() {
        let cells: Vec<String> = result
            .methods
            .iter()
            .map(|(method, steps)| {
                let median = crate::median(&steps[row]);
                let times = match worktree {
                    Some((_, base)) if *method != Method::Worktree && !base[row].is_empty() => {
                        let base = crate::median(&base[row]).as_secs_f64();
                        if base > 0.0 { format!(" ({:.1}x)", median.as_secs_f64() / base) } else { String::new() }
                    }
                    _ => String::new(),
                };
                format!("{}{times}", crate::fmt_duration(median))
            })
            .collect();
        println!("| {step} | {} |", cells.join(" | "));
    }
}

pub fn json(result: &CopiesResult) -> serde_json::Value {
    let rows: Vec<_> = STEPS
        .iter()
        .enumerate()
        .map(|(row, step)| {
            let nanos: serde_json::Map<String, serde_json::Value> = result
                .methods
                .iter()
                .map(|(method, steps)| (method.name().to_owned(), serde_json::json!(crate::nanos(&steps[row]))))
                .collect();
            serde_json::json!({ "step": step, "nanos": nanos })
        })
        .collect();
    serde_json::json!({ "package": result.package, "rows": rows })
}
