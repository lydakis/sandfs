//! `sandfs-bench`: time the same file workloads on sandfs and on what it replaces.
//!
//! Targets are the host filesystem used directly (the floor), sandfs through its Rust
//! API, Linux overlayfs (the kernel's copy-on-write filesystem, when this process can
//! mount it) and the `sandfs` command's FUSE mount (when FUSE is available). A second
//! table compares a slow, lazily read source with copying it all up front.
//!
//! Every timed run starts from a fresh view: a new workspace or mount, so caches start
//! empty except the kernel's page cache, which is warm for every target alike.

mod data;
mod lazy;
mod targets;
mod workloads;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
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
    let scratch = args.scratch.clone().unwrap_or_else(std::env::temp_dir);
    eprintln!("generating the data set in {}", scratch.display());
    let data = Data::generate(scale, &scratch)?;
    let root = data.root().to_owned();

    let wanted = |target: TargetArg| args.targets.as_ref().is_none_or(|list| list.contains(&target));
    let explicit = |target: TargetArg| args.targets.as_ref().is_some_and(|list| list.contains(&target));
    let mut targets: Vec<Box<dyn Target>> = Vec::new();
    let mut skipped = Vec::new();
    if wanted(TargetArg::Host) {
        targets.push(Box::new(Host { data: root.clone(), scratch: scratch.clone() }));
    }
    if wanted(TargetArg::Sandfs) {
        targets.push(Box::new(Sandfs { data: root.clone() }));
    }
    if wanted(TargetArg::Overlayfs) {
        if Overlay::available(&scratch) {
            targets.push(Box::new(Overlay { data: root.clone(), scratch: scratch.clone() }));
        } else if explicit(TargetArg::Overlayfs) {
            bail!("overlayfs cannot be mounted here (it needs root)");
        } else {
            skipped.push("overlayfs (needs root)".to_owned());
        }
    }
    if wanted(TargetArg::Fuse) {
        let bin = match &args.sandfs_bin {
            Some(bin) => bin.clone(),
            None => std::env::current_exe()?.with_file_name("sandfs"),
        };
        match Fuse::new(root.clone(), scratch.clone(), bin) {
            Ok(fuse) => targets.push(Box::new(fuse)),
            Err(err) if explicit(TargetArg::Fuse) => return Err(err),
            Err(err) => skipped.push(format!("sandfs FUSE ({err})")),
        }
    }

    let selected = |name: &str| args.only.as_ref().is_none_or(|only| name.contains(only.as_str()));
    let workloads: Vec<_> = workloads::all(&data).into_iter().filter(|w| selected(w.name)).collect();
    if workloads.is_empty() && !lazy::NAMES.iter().any(|name| selected(name)) {
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

    print_report(&args, &data, &targets, &skipped, &workloads, &table, &lazy);
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
        let report = json!({ "scale": format!("{:?}", args.scale), "workloads": rows, "lazy": lazy_rows });
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(())
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
    sorted[sorted.len() / 2]
}

fn fmt_duration(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
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
