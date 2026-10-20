//! `sandfs-bench`: time the same file workloads on sandfs and on what it replaces.
//!
//! Targets are the host filesystem used directly (the floor), sandfs through its Rust
//! API, Linux overlayfs (the kernel's copy-on-write filesystem, when this process can
//! mount it) and the `sandfs` command's FUSE mount (when FUSE is available). A second
//! table compares a slow, lazily read source with copying it all up front, a third
//! runs a whole task (search, edit, check, list changes) in several parallel attempts,
//! and with `--remote URL` a fourth mounts a large repository from a git server and
//! compares it with cloning it.
//!
//! Every timed run starts from a fresh view: a new workspace or mount, so caches start
//! empty except the kernel's page cache, which is warm for every target alike.

mod build;
mod data;
mod everyday;
mod lazy;
mod remote;
mod targets;
mod workflow;
mod workloads;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use serde_json::json;

use crate::data::{Data, Scale};
use crate::targets::{Fuse, Host, Overlay, Sandfs, Target};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ScaleArg {
    Full,
    Quick,
    Tiny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TargetArg {
    Host,
    Sandfs,
    Overlayfs,
    Fuse,
}

#[derive(Parser)]
#[command(name = "sandfs-bench", about = "Benchmark sandfs against the host filesystem and overlayfs")]
struct Args {
    /// Data set size and repetitions.
    #[arg(long, value_enum, default_value = "full")]
    scale: ScaleArg,
    /// Targets to run, comma-separated: host, sandfs, overlayfs, fuse. Default: every
    /// target available here.
    #[arg(long, value_enum, value_delimiter = ',')]
    targets: Option<Vec<TargetArg>>,
    /// Only run workloads whose name contains this.
    #[arg(long)]
    only: Option<String>,
    /// Timed runs per workload (default depends on the scale).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    iters: Option<u64>,
    /// Parallel attempts in the workflow benchmark.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..))]
    attempts: u64,
    /// Delay per call of the slow source, in microseconds.
    #[arg(long, default_value_t = 1000)]
    latency_us: u64,
    /// Where to generate the data set and mounts (default: the system temp dir).
    #[arg(long)]
    scratch: Option<PathBuf>,
    /// The `sandfs` binary for the FUSE target (default: next to this binary).
    #[arg(long)]
    sandfs_bin: Option<PathBuf>,
    /// Also write every sample as JSON to this file.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Run only the workflow, for the one target given, and report it as JSON. The
    /// workflow runs each target this way so its memory peak is its own.
    #[arg(long, hide = true)]
    workflow_child: bool,
    /// A git repository URL to mount with sandfs and compare with cloning it; needs the
    /// network. `--only remote` runs just this table.
    #[arg(long)]
    remote: Option<String>,
    /// The folder of the remote repository to search (default: the largest folder of
    /// at most 1,000 files).
    #[arg(long)]
    remote_folder: Option<String>,
    /// Ways to get the remote repository's files, comma-separated: sandfs, sparse,
    /// shallow, blobless, full. Default: all of them.
    #[arg(long, value_enum, value_delimiter = ',')]
    remote_methods: Option<Vec<remote::Method>>,
    /// The package of this repository the build table builds.
    #[arg(long, default_value = "sandfs-cli")]
    build_package: String,
    /// Run `--build-command` in copies of this folder instead of building a package of
    /// this repository.
    #[arg(long, requires = "build_command")]
    build_dir: Option<PathBuf>,
    /// The shell command the build table runs in each copy of `--build-dir`, such as
    /// `go test ./...` or `npm test`. Each column runs it with its own empty
    /// `XDG_CACHE_HOME`; give a tool that keeps its cache elsewhere one inside the folder.
    #[arg(long, requires = "build_dir")]
    build_command: Option<String>,
    /// A file inside `--build-dir` the build table changes (adding an empty line) before
    /// each run after the first.
    #[arg(long, requires = "build_dir")]
    build_change: Option<PathBuf>,
    /// Run one remote method against listings already fetched to this directory and
    /// report it as JSON, so its memory peak is its own.
    #[arg(long, hide = true, requires = "remote_child_method")]
    remote_child: Option<PathBuf>,
    #[arg(long, hide = true, value_enum)]
    remote_child_method: Option<remote::Method>,
}

fn main() {
    if let Err(err) = run(Args::parse()) {
        eprintln!("sandfs-bench: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let mut scale = match args.scale {
        ScaleArg::Full => Scale::FULL,
        ScaleArg::Quick => Scale::QUICK,
        ScaleArg::Tiny => Scale::TINY,
    };
    if let Some(iters) = args.iters {
        scale.iters = iters as usize;
    }
    let bin = match &args.sandfs_bin {
        Some(bin) => bin.clone(),
        None => std::env::current_exe()?.with_file_name("sandfs"),
    };
    if let (Some(plan_dir), Some(method)) = (&args.remote_child, args.remote_child_method) {
        let url = args.remote.as_deref().context("--remote-child needs --remote")?;
        let plan = remote::Plan::read(&plan_dir.with_file_name("plan.json"))?;
        ensure!(plan.url == url, "the plan is for {}, not {url}", plan.url);
        let root = plan_dir.with_file_name(method.arg());
        std::fs::create_dir(&root)?;
        let result = remote::run(&plan, method, &root, &bin)?;
        let path = args.json.as_ref().context("--remote-child needs --json")?;
        std::fs::write(path, serde_json::to_string(&remote_json(&result))?)?;
        return Ok(());
    }
    // A directory of this run's own, so the workflow's disk count sees nothing else.
    let parent = args.scratch.clone().unwrap_or_else(std::env::temp_dir);
    let run_dir = tempfile::Builder::new().prefix("sandfs-bench-").tempdir_in(&parent)?;
    let scratch = run_dir.path().to_owned();
    if args.only.as_deref() == Some("remote") {
        let url = args.remote.as_deref().context("--only remote needs --remote URL")?;
        let (plan, results) = run_remote(&args, url, &scratch)?;
        print_remote(&plan, &results);
        if let Some(path) = &args.json {
            let rows: Vec<_> = results.iter().map(remote_json).collect();
            std::fs::write(path, serde_json::to_string_pretty(&json!({ "remote": rows }))?)?;
        }
        return Ok(());
    }
    let wanted = |target: TargetArg| args.targets.as_ref().is_none_or(|list| list.contains(&target));
    let job = match (&args.build_dir, &args.build_command) {
        (Some(dir), Some(command)) => {
            build::Job::Command { dir: dir.clone(), command: command.clone(), change: args.build_change.clone() }
        }
        _ => build::Job::Cargo { package: args.build_package.clone() },
    };
    // The build table needs no data set, so it runs on its own.
    if args.only.as_deref() == Some(build::NAME) {
        let named = args.targets.as_ref().is_some_and(|list| list.contains(&TargetArg::Fuse));
        let (host, mount) = (wanted(TargetArg::Host), (wanted(TargetArg::Fuse), named));
        let built = build::run(&job, &scratch, &bin, scale.iters, host, mount)?;
        build::print(&built);
        if let Some(path) = &args.json {
            std::fs::write(path, serde_json::to_string_pretty(&json!({ "build": build::json(&built) }))?)?;
        }
        return Ok(());
    }
    eprintln!("generating the data set in {}", scratch.display());
    let data = Data::generate(scale, &scratch)?;
    let root = data.root().to_owned();

    let explicit = |target: TargetArg| args.targets.as_ref().is_some_and(|list| list.contains(&target));
    let mut targets: Vec<Box<dyn Target>> = Vec::new();
    // The kind of each target, in the same order, for running it in a child process.
    let mut kinds = Vec::new();
    let mut skipped = Vec::new();
    if wanted(TargetArg::Host) {
        targets.push(Box::new(Host { data: root.clone(), scratch: scratch.clone() }));
        kinds.push(TargetArg::Host);
    }
    if wanted(TargetArg::Sandfs) {
        targets.push(Box::new(Sandfs { data: root.clone() }));
        kinds.push(TargetArg::Sandfs);
    }
    if wanted(TargetArg::Overlayfs) {
        if Overlay::available(&scratch) {
            targets.push(Box::new(Overlay { data: root.clone(), scratch: scratch.clone() }));
            kinds.push(TargetArg::Overlayfs);
        } else if explicit(TargetArg::Overlayfs) {
            bail!("overlayfs cannot be mounted here (it needs root)");
        } else {
            skipped.push("overlayfs (needs root)".to_owned());
        }
    }
    if wanted(TargetArg::Fuse) {
        match Fuse::new(root.clone(), scratch.clone(), bin.clone()) {
            Ok(fuse) => {
                targets.push(Box::new(fuse));
                kinds.push(TargetArg::Fuse);
            }
            Err(err) if explicit(TargetArg::Fuse) => return Err(err),
            Err(err) => skipped.push(format!("sandfs FUSE ({err})")),
        }
    }

    let selected = |name: &str| args.only.as_ref().is_none_or(|only| name.contains(only.as_str()));
    let workloads: Vec<_> = workloads::all(&data).into_iter().filter(|w| selected(w.name)).collect();
    let run_workflow = selected(WORKFLOW);
    let run_everyday = everyday::Op::ALL.iter().any(|op| selected(&format!("everyday/{}", op.name())));
    // The build table has host and FUSE columns only; a run of other targets leaves it out.
    let run_build = selected(build::NAME) && (wanted(TargetArg::Host) || wanted(TargetArg::Fuse));
    if workloads.is_empty()
        && !lazy::NAMES.iter().any(|name| selected(name))
        && !run_workflow
        && !run_everyday
        && !run_build
    {
        bail!("--only {:?} matches no workload", args.only.as_deref().unwrap_or_default());
    }
    let mut table: Vec<Vec<Option<Vec<Duration>>>> = Vec::new();
    for workload in &workloads {
        eprintln!("{}", workload.name);
        let mut row = Vec::new();
        for target in &targets {
            row.push(Some(measure(target.as_ref(), workload, &data)?));
        }
        table.push(row);
    }
    let lazy = lazy::run(&data, Duration::from_micros(args.latency_us), scale.iters, &selected)?;
    let mut flows = Vec::new();
    if run_workflow {
        eprintln!("{WORKFLOW}");
        for (target, kind) in targets.iter().zip(&kinds) {
            let runs = if args.workflow_child {
                (0..scale.iters)
                    .map(|_| workflow::run(target.as_ref(), &data, &scratch, args.attempts as usize))
                    .collect::<Result<Vec<_>>>()?
            } else {
                workflow_in_child(&args, *kind, target.name(), scale.iters, &scratch)?
            };
            flows.push(runs);
        }
    }
    if args.workflow_child {
        let path = args.json.as_ref().context("--workflow-child needs --json")?;
        std::fs::write(path, serde_json::to_string(&workflow_json(&flows))?)?;
        return Ok(());
    }

    let mut days = Vec::new();
    if run_everyday {
        for group in everyday_groups(&args, &data, &scratch, &bin)? {
            days.push(everyday::run(&group, &selected)?);
        }
    }

    let remote = match &args.remote {
        Some(url) if selected("remote") => Some(run_remote(&args, url, &scratch)?),
        _ => None,
    };
    let built = if run_build {
        let (host, mount) = (wanted(TargetArg::Host), (wanted(TargetArg::Fuse), explicit(TargetArg::Fuse)));
        Some(build::run(&job, &scratch, &bin, scale.iters, host, mount)?)
    } else {
        None
    };

    print_report(&args, &data, &targets, &skipped, &workloads, &table, &lazy);
    print_workflow(&args, &flows);
    for day in &days {
        everyday::print(day);
    }
    if let Some((plan, results)) = &remote {
        print_remote(plan, results);
    }
    if let Some(built) = &built {
        build::print(built);
    }
    if let Some(path) = &args.json {
        let rows: Vec<_> = workloads
            .iter()
            .zip(&table)
            .map(|(workload, row)| {
                let cells: serde_json::Map<_, _> = targets
                    .iter()
                    .zip(row)
                    .map(|(target, cell)| (target.name().to_owned(), json!(cell.as_ref().map(|t| nanos(t)))))
                    .collect();
                json!({ "workload": workload.name, "what": workload.what, "nanos": cells })
            })
            .collect();
        let lazy_rows: Vec<_> = lazy
            .iter()
            .map(|result| {
                json!({
                    "workload": result.name,
                    "what": result.what,
                    "lazy": result.lazy.iter().map(|(t, calls)| json!({"nanos": t.as_nanos() as u64, "calls": calls})).collect::<Vec<_>>(),
                    "eager": result.eager.iter().map(|(t, calls)| json!({"nanos": t.as_nanos() as u64, "calls": calls})).collect::<Vec<_>>(),
                })
            })
            .collect();
        let report = json!({
            "scale": format!("{:?}", args.scale),
            "workloads": rows,
            "lazy": lazy_rows,
            "workflow": workflow_json(&flows),
            "everyday": days.iter().map(everyday::json).collect::<Vec<_>>(),
            "remote": remote.iter().flat_map(|(_, results)| results.iter().map(remote_json)).collect::<Vec<_>>(),
            "build": built.as_ref().map(build::json),
        });
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(())
}

const WORKFLOW: &str = "workflow/attempts";

/// The sources the everyday operations run on, each with the ways of reaching it that
/// work here and `--targets` selects: the data set as a host folder, a slow source
/// holding part of it, and the `--remote` repository. The Rust API columns count as
/// `sandfs` and the mount columns as `fuse`.
fn everyday_groups(
    args: &Args,
    data: &Data,
    scratch: &std::path::Path,
    bin: &std::path::Path,
) -> Result<Vec<everyday::Group>> {
    let wanted = |target: TargetArg| args.targets.as_ref().is_none_or(|list| list.contains(&target));
    let mount = wanted(TargetArg::Fuse) && {
        let mount = everyday::can_mount(scratch, bin);
        if let Err(err) = &mount {
            eprintln!("everyday: no mount columns ({err:#})");
        }
        mount.is_ok()
    };
    let api = wanted(TargetArg::Sandfs);
    let root = data.root().to_owned();
    let mut folder: Vec<Box<dyn Target>> = Vec::new();
    if wanted(TargetArg::Host) {
        folder.push(Box::new(Host { data: root.clone(), scratch: scratch.to_owned() }));
    }
    if api {
        folder.push(Box::new(Sandfs { data: root.clone() }));
    }
    if mount {
        folder.push(Box::new(Fuse::new(root, scratch.to_owned(), bin.to_owned())?));
    }
    let s = data.scale;
    let latency = Duration::from_micros(args.latency_us);
    let mut groups = vec![
        everyday::Group {
            source: format!("a host folder ({} small files below small/)", s.small_files()),
            paths: everyday::data_paths(data, usize::MAX),
            columns: folder,
            iters: s.iters,
        },
        everyday::Group {
            source: format!(
                "a source taking {} µs per call ({} small files below small/)",
                latency.as_micros(),
                s.lazy_files.min(s.small_files())
            ),
            paths: everyday::data_paths(data, s.lazy_files),
            columns: if api {
                vec![Box::new(everyday::SlowSource::new(data, s.lazy_files, latency)?)]
            } else {
                Vec::new()
            },
            iters: s.iters,
        },
    ];
    if let Some(url) = args.remote.as_ref().filter(|_| api || mount) {
        let listings = scratch.join("everyday-listings");
        let plan = remote::Plan::fetch(url, args.remote_folder.as_deref(), &listings)?;
        let mut columns: Vec<Box<dyn Target>> = Vec::new();
        if api {
            columns.push(Box::new(everyday::RemoteLibrary {
                url: url.clone(),
                commit: plan.commit.clone(),
                scratch: scratch.to_owned(),
            }));
        }
        if mount {
            columns.push(Box::new(everyday::RemoteMount {
                url: url.clone(),
                commit: plan.commit.clone(),
                scratch: scratch.to_owned(),
                bin: bin.to_owned(),
            }));
        }
        let short = &plan.commit[..plan.commit.len().min(12)];
        groups.push(everyday::Group {
            source: format!("{url} at {short} ({} files), nothing fetched before each run", plan.files),
            paths: everyday::remote_paths(&plan),
            columns,
            iters: 1,
        });
    }
    groups.retain(|group| !group.columns.is_empty());
    Ok(groups)
}

/// Fetch the remote repository's listings once, then run each method in a fresh copy
/// of this program, so the memory peak it reports is its own. Every method must read
/// the same bytes and find the same matches.
fn run_remote(args: &Args, url: &str, scratch: &std::path::Path) -> Result<(remote::Plan, Vec<remote::RemoteResult>)> {
    let base = scratch.join("remote");
    std::fs::create_dir(&base)?;
    let plan_dir = base.join("listings");
    let plan = remote::Plan::fetch(url, args.remote_folder.as_deref(), &plan_dir)?;
    plan.save(&base.join("plan.json"))?;
    let methods = args.remote_methods.clone().unwrap_or_else(|| remote::Method::value_variants().to_vec());
    let mut results: Vec<remote::RemoteResult> = Vec::new();
    for method in methods {
        eprintln!("remote/{}", method.arg());
        let report = base.join(format!("{}.json", method.arg()));
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--remote")
            .arg(url)
            .arg("--remote-child")
            .arg(&plan_dir)
            .args(["--remote-child-method", method.arg(), "--json"])
            .arg(&report);
        if let Some(folder) = &args.remote_folder {
            command.arg("--remote-folder").arg(folder);
        }
        if let Some(bin) = &args.sandfs_bin {
            command.arg("--sandfs-bin").arg(bin);
        }
        let status = command.status().context("starting the remote benchmark process")?;
        if !status.success() {
            bail!("the remote benchmark for {} failed ({status})", method.name());
        }
        let row: serde_json::Value = serde_json::from_slice(&std::fs::read(&report)?)?;
        let result = remote_from_json(method, &row)?;
        if let Some(first) = results.first() {
            if (result.bytes, result.matches) != (first.bytes, first.matches) {
                bail!(
                    "{} read {} bytes with {} matches, but {} read {} bytes with {} matches",
                    method.name(),
                    result.bytes,
                    result.matches,
                    first.method.name(),
                    first.bytes,
                    first.matches
                );
            }
        }
        results.push(result);
        // Each method's copy can be large; only its numbers are kept.
        std::fs::remove_dir_all(base.join(method.arg()))?;
    }
    Ok((plan, results))
}

fn remote_json(result: &remote::RemoteResult) -> serde_json::Value {
    json!({
        "method": result.method.arg(),
        "ready_nanos": result.ready.as_nanos() as u64,
        "first_nanos": result.first.as_nanos() as u64,
        "samples_nanos": result.samples.as_nanos() as u64,
        "search_nanos": result.search.as_nanos() as u64,
        "status_nanos": result.status.as_nanos() as u64,
        "commit_nanos": result.commit.as_nanos() as u64,
        "status_again_nanos": result.status_again.as_nanos() as u64,
        "fork_nanos": result.fork.as_nanos() as u64,
        "grep_nanos": result.grep.map(|grep| grep.map(|grep| grep.as_nanos() as u64)),
        "grep_stopped": result.grep.is_some_and(|grep| grep.is_none()),
        "matches": result.matches,
        "bytes": result.bytes,
        "peak_rss_bytes": result.peak_rss,
        "disk_bytes": result.disk,
    })
}

fn remote_from_json(method: remote::Method, row: &serde_json::Value) -> Result<remote::RemoteResult> {
    let number = |key: &str| row[key].as_u64().with_context(|| format!("remote JSON has no {key}"));
    let nanos = |key: &str| number(key).map(Duration::from_nanos);
    Ok(remote::RemoteResult {
        method,
        ready: nanos("ready_nanos")?,
        first: nanos("first_nanos")?,
        samples: nanos("samples_nanos")?,
        search: nanos("search_nanos")?,
        status: nanos("status_nanos")?,
        commit: nanos("commit_nanos")?,
        status_again: nanos("status_again_nanos")?,
        fork: nanos("fork_nanos")?,
        grep: match (&row["grep_nanos"], row["grep_stopped"].as_bool()) {
            (_, Some(true)) => Some(None),
            (nanos, _) => nanos.as_u64().map(|nanos| Some(Duration::from_nanos(nanos))),
        },
        matches: usize::try_from(number("matches")?)?,
        bytes: number("bytes")?,
        peak_rss: number("peak_rss_bytes")?,
        disk: number("disk_bytes")?,
    })
}

fn print_remote(plan: &remote::Plan, results: &[remote::RemoteResult]) {
    let short = &plan.commit[..plan.commit.len().min(12)];
    println!();
    println!(
        "{} at {short}: {} files. Each method starts with nothing local, reads one file, then {} more spread across the tree, then searches {}/ ({} files), then makes a second copy to work in. The sparse checkout is told in advance exactly which files will be read. One run each.",
        plan.url,
        plan.files,
        remote::SAMPLES,
        plan.subtree,
        plan.subtree_files.len()
    );
    println!();
    println!(
        "| method | ready | first file | {} files | search {}/ | second copy | peak memory | on disk |",
        remote::SAMPLES,
        plan.subtree
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for result in results {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            result.method.name(),
            fmt_duration(result.ready),
            fmt_duration(result.first),
            fmt_duration(result.samples),
            fmt_duration(result.search),
            fmt_duration(result.fork),
            fmt_bytes(result.peak_rss),
            fmt_bytes(result.disk),
        );
    }
    println!();
    println!(
        "Then, in the same copy: the first `git status`, committing one changed file in {}/, `git status` again, and `grep -r` over the whole copy for a word it doesn't hold. Memory and disk above include these steps, except the disk the search fills.",
        plan.subtree
    );
    println!();
    println!("| method | first git status | commit | git status again | grep -r everything |");
    println!("|---|---:|---:|---:|---:|");
    for result in results {
        println!(
            "| {} | {} | {} | {} | {} |",
            result.method.name(),
            fmt_duration(result.status),
            fmt_duration(result.commit),
            fmt_duration(result.status_again),
            match result.grep {
                None => "not held".to_owned(),
                Some(None) => format!("over {}", fmt_duration(remote::GREP_LIMIT)),
                Some(Some(grep)) => fmt_duration(grep),
            },
        );
    }
}

/// Every workflow run as JSON rows.
fn workflow_json(flows: &[Vec<workflow::WorkflowResult>]) -> serde_json::Value {
    let rows: Vec<_> = flows
        .iter()
        .flatten()
        .map(|run| {
            json!({
                "target": run.target,
                "attempts": run.steps.len(),
                "forks_nanos": run.forks.as_nanos() as u64,
                "wall_nanos": run.wall.as_nanos() as u64,
                "steps_nanos": run.steps.iter().map(|steps| steps.map(|t| t.as_nanos() as u64)).collect::<Vec<_>>(),
                "peak_rss_bytes": run.peak_rss,
                "disk_bytes": run.disk,
            })
        })
        .collect();
    json!(rows)
}

/// Run the workflow for one target in a fresh copy of this program, so the memory
/// peak it reports holds only that target's work, then read back its runs.
fn workflow_in_child(
    args: &Args,
    kind: TargetArg,
    name: &'static str,
    iters: usize,
    scratch: &std::path::Path,
) -> Result<Vec<workflow::WorkflowResult>> {
    let report = scratch.join(format!("workflow-{name}.json"));
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--workflow-child")
        .args(["--only", "workflow", "--scale"])
        .arg(args.scale.to_possible_value().context("scale name")?.get_name())
        .args(["--targets", kind.to_possible_value().context("target name")?.get_name()])
        .arg(format!("--iters={iters}"))
        .arg(format!("--attempts={}", args.attempts))
        .arg("--scratch")
        .arg(scratch)
        .arg("--json")
        .arg(&report);
    if let Some(bin) = &args.sandfs_bin {
        command.arg("--sandfs-bin").arg(bin);
    }
    let status = command.status().context("starting the workflow process")?;
    if !status.success() {
        bail!("the workflow process for {name} failed ({status})");
    }
    let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&report)?)?;
    std::fs::remove_file(&report)?;
    let nanos = |value: &serde_json::Value| value.as_u64().map(Duration::from_nanos).context("workflow JSON");
    rows.as_array()
        .context("workflow JSON")?
        .iter()
        .map(|row| {
            let steps = row["steps_nanos"]
                .as_array()
                .context("workflow JSON")?
                .iter()
                .map(|steps| {
                    let steps = steps.as_array().context("workflow JSON")?;
                    let step = |i: usize| steps.get(i).context("workflow JSON").and_then(nanos);
                    Ok([step(0)?, step(1)?, step(2)?, step(3)?])
                })
                .collect::<Result<_>>()?;
            Ok(workflow::WorkflowResult {
                target: name,
                forks: nanos(&row["forks_nanos"])?,
                wall: nanos(&row["wall_nanos"])?,
                steps,
                peak_rss: row["peak_rss_bytes"].as_u64().context("workflow JSON")?,
                disk: row["disk_bytes"].as_u64().context("workflow JSON")?,
            })
        })
        .collect()
}

/// The workflow table: per target, the median run by wall time, with every attempt's
/// step times pooled across runs.
fn print_workflow(args: &Args, flows: &[Vec<workflow::WorkflowResult>]) {
    if flows.is_empty() {
        return;
    }
    let attempts = args.attempts;
    println!();
    println!(
        "{attempts} parallel attempts, each on its own fork: search every small file, rewrite up to {} files, add {} and delete {}, `cksum` part of the tree, list the changes. Median of runs; step times are medians over attempts.",
        workflow::REWRITES,
        workflow::NOTES,
        workflow::DELETES
    );
    println!();
    println!(
        "| target | {attempts} attempts, wall | forks | search | edit | check | changes | peak memory | left on disk |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for runs in flows {
        let walls: Vec<Duration> = runs.iter().map(|run| run.wall).collect();
        let forks: Vec<Duration> = runs.iter().map(|run| run.forks).collect();
        let step = |i: usize| {
            let times: Vec<Duration> =
                runs.iter().flat_map(|run| run.steps.iter().map(move |steps| steps[i])).collect();
            fmt_duration(median(&times))
        };
        let peak = runs.iter().map(|run| run.peak_rss).max().unwrap_or(0);
        let disk = runs.iter().map(|run| run.disk).max().unwrap_or(0);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            runs[0].target,
            fmt_duration(median(&walls)),
            fmt_duration(median(&forks)),
            step(0),
            step(1),
            step(2),
            step(3),
            fmt_bytes(peak),
            fmt_bytes(disk),
        );
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / f64::from(1 << 20);
    if mib < 10.0 { format!("{mib:.1} MiB") } else { format!("{mib:.0} MiB") }
}

/// Time `scale.iters` runs of `workload` on `target`, each on a fresh view.
fn measure(target: &dyn Target, workload: &workloads::Workload, data: &Data) -> Result<Vec<Duration>> {
    let mut times = Vec::new();
    for _ in 0..data.scale.iters {
        let mut view = target.open(workload.writable)?;
        (workload.prepare)(data, view.as_mut())?;
        let start = Instant::now();
        let leftover = (workload.run)(data, view.as_mut());
        let elapsed = start.elapsed();
        // A fork is checked itself, so a target that forks without the changes fails.
        let mut leftover = leftover?;
        (workload.check)(data, leftover.as_deref_mut().unwrap_or(view.as_mut()))?;
        drop(leftover);
        drop(view);
        times.push(elapsed);
    }
    Ok(times)
}

fn nanos(times: &[Duration]) -> Vec<u64> {
    times.iter().map(|t| t.as_nanos() as u64).collect()
}

fn median(times: &[Duration]) -> Duration {
    let mut sorted = times.to_vec();
    sorted.sort();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 { (sorted[mid - 1] + sorted[mid]) / 2 } else { sorted[mid] }
}

fn fmt_duration(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1.0 {
        format!("{} ns", d.as_nanos())
    } else if us < 10.0 {
        format!("{us:.1} µs")
    } else if us < 1000.0 {
        format!("{us:.0} µs")
    } else if us < 1e6 {
        format!("{:.1} ms", us / 1e3)
    } else {
        format!("{:.2} s", us / 1e6)
    }
}

fn print_report(
    args: &Args,
    data: &Data,
    targets: &[Box<dyn Target>],
    skipped: &[String],
    workloads: &[workloads::Workload],
    table: &[Vec<Option<Vec<Duration>>>],
    lazy: &[lazy::LazyResult],
) {
    let s = data.scale;
    println!(
        "Scale {:?}: {} small files ({:.0} MiB), a {}-entry directory, a {}-level chain, a {} MiB file; median of {} runs.",
        args.scale,
        s.small_files(),
        data.small_bytes as f64 / f64::from(1 << 20),
        s.wide,
        s.depth,
        s.large_mib,
        s.iters
    );
    if !skipped.is_empty() {
        println!("Skipped: {}.", skipped.join(", "));
    }
    if !workloads.is_empty() {
        println!();
        let header: Vec<&str> = targets.iter().map(|t| t.name()).collect();
        println!("| workload | what | {} |", header.join(" | "));
        println!("|---|---|{}", "---:|".repeat(targets.len()));
        for (workload, row) in workloads.iter().zip(table) {
            let cells: Vec<String> =
                row.iter().map(|cell| cell.as_ref().map_or("-".to_owned(), |t| fmt_duration(median(t)))).collect();
            println!("| {} | {} | {} |", workload.name, workload.what, cells.join(" | "));
        }
    }
    if !lazy.is_empty() {
        println!();
        println!("| workload | what | sandfs (lazy) | copy up front |");
        println!("|---|---|---:|---:|");
        for result in lazy {
            let cell = |runs: &[(Duration, usize)]| {
                let times: Vec<Duration> = runs.iter().map(|(t, _)| *t).collect();
                let calls = runs.first().map_or(0, |(_, calls)| *calls);
                format!("{} ({calls} calls)", fmt_duration(median(&times)))
            };
            println!("| {} | {} | {} | {} |", result.name, result.what, cell(&result.lazy), cell(&result.eager));
        }
    }
}
