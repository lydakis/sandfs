//! A whole task, several times in parallel: what a harness running attempts side by
//! side pays, end to end.
//!
//! Each attempt gets its own fork of one prepared view, then
//! 1. searches every small file for a byte pattern,
//! 2. edits what it found: rewrites matching files, adds notes and deletes a few,
//! 3. checks its work by running `cksum` over part of the tree (on targets that are
//!    real directories; through the Rust API the same files are read in-process),
//! 4. lists its changes, which is also when the FUSE mount saves them.
//!
//! Forks are made one after another, as a harness would start attempts; the rest runs
//! on one thread per attempt. Besides time, the report shows the peak memory of this
//! process plus any serving processes, and the bytes the attempts left on disk.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::data::Data;
use crate::targets::{Target, View};

/// Files each attempt rewrites, adds and deletes.
pub const REWRITES: usize = 50;
pub const NOTES: usize = 10;
pub const DELETES: usize = 5;
/// Directories whose files the check step runs over.
const CHECKED_DIRS: usize = 2;

/// One target's result.
pub struct WorkflowResult {
    pub target: &'static str,
    /// Making every fork, one after another.
    pub forks: Duration,
    /// From the first fork to the last attempt finishing.
    pub wall: Duration,
    /// Per-attempt durations of search, edit, check and changes.
    pub steps: Vec<[Duration; 4]>,
    /// Peak resident memory of this process and the target's serving processes.
    pub peak_rss: u64,
    /// Bytes on disk the attempts added under the scratch directory.
    pub disk: u64,
}

/// What every attempt must find and leave behind, worked out from the data set.
struct Plan {
    needle: Vec<u8>,
    matches: usize,
    rewrites: Vec<String>,
    deletes: Vec<String>,
    checked: Vec<String>,
}

impl Plan {
    fn new(data: &Data) -> Result<Plan> {
        // Three bytes from the first file: rare enough to match a few dozen files.
        let first = std::fs::read(data.root().join(&data.small[0]))?;
        let needle = first[100..103].to_vec();
        let mut found = Vec::new();
        for path in &data.small {
            if contains(&std::fs::read(data.root().join(path))?, &needle) {
                found.push(path.clone());
            }
        }
        let matches = found.len();
        // Rewrite matches first, then fill up with other files so the count is fixed.
        let mut rewrites: Vec<String> = found.into_iter().take(REWRITES).collect();
        for path in data.spread(REWRITES * 2) {
            if rewrites.len() == REWRITES.min(data.small.len() / 2) {
                break;
            }
            if !rewrites.iter().any(|p| p == path) {
                rewrites.push(path.to_owned());
            }
        }
        let deletes: Vec<String> =
            data.small.iter().rev().filter(|p| !rewrites.contains(p)).take(DELETES).cloned().collect();
        let checked_dirs: Vec<&String> = data.small_dirs.iter().skip(1).take(CHECKED_DIRS).collect();
        let checked = data
            .small
            .iter()
            .filter(|p| checked_dirs.iter().any(|d| p.starts_with(&format!("{d}/"))) && !deletes.contains(p))
            .chain(rewrites.iter())
            .cloned()
            .collect();
        Ok(Plan { needle, matches, rewrites, deletes, checked })
    }

    fn notes(&self, data: &Data, attempt: usize) -> Vec<String> {
        (0..NOTES).map(|i| format!("{}/note-{attempt}-{i}.txt", data.small_dirs[i % data.small_dirs.len()])).collect()
    }

    fn changed(&self) -> usize {
        self.rewrites.len() + NOTES + self.deletes.len()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// One attempt after its fork: the four timed steps.
fn attempt(data: &Data, plan: &Plan, index: usize, view: &mut dyn View) -> Result<[Duration; 4]> {
    let start = Instant::now();
    let mut found = 0;
    for path in &data.small {
        if contains(&view.contents(path)?, &plan.needle) {
            found += 1;
        }
    }
    ensure!(found == plan.matches, "search found {found} files, expected {}", plan.matches);
    let search = start.elapsed();

    let start = Instant::now();
    for path in &plan.rewrites {
        let mut bytes = view.contents(path)?;
        bytes.extend_from_slice(format!("\nreviewed by attempt {index}\n").as_bytes());
        view.write(path, &bytes)?;
    }
    for (i, path) in plan.notes(data, index).iter().enumerate() {
        view.write(path, format!("attempt {index}, note {i}\n").as_bytes())?;
    }
    for path in &plan.deletes {
        view.remove(path)?;
    }
    let edit = start.elapsed();

    let start = Instant::now();
    let checked = match view.root_dir() {
        Some(root) => cksum(root, &plan.checked)?,
        None => plan.checked.iter().map(|path| view.contents(path).map(drop)).collect::<Result<Vec<_>>>()?.len(),
    };
    ensure!(checked == plan.checked.len(), "checked {checked} files, expected {}", plan.checked.len());
    let check = start.elapsed();

    let start = Instant::now();
    let changed = view.changes()?;
    ensure!(changed == plan.changed(), "found {changed} changes, expected {}", plan.changed());
    let changes = start.elapsed();
    Ok([search, edit, check, changes])
}

/// Run `cksum` over `files` below `root` as one process; returns the files it summed.
fn cksum(root: &Path, files: &[String]) -> Result<usize> {
    let output = Command::new("cksum").current_dir(root).args(files).output().context("running cksum")?;
    if !output.status.success() {
        bail!("cksum failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout.split(|b| *b == b'\n').filter(|line| !line.is_empty()).count())
}

/// Run `attempts` attempts on `target`.
pub fn run(target: &dyn Target, data: &Data, scratch: &Path, attempts: usize) -> Result<WorkflowResult> {
    let plan = Plan::new(data)?;
    let mut base = target.open(true)?;
    let disk_before = disk_usage(scratch)?;
    reset_peak_rss();

    let start = Instant::now();
    let mut views = Vec::with_capacity(attempts);
    for _ in 0..attempts {
        views.push(base.fork()?);
    }
    let forks = start.elapsed();
    let steps = std::thread::scope(|scope| {
        let handles: Vec<_> = views
            .iter_mut()
            .enumerate()
            .map(|(index, view)| {
                let plan = &plan;
                scope.spawn(move || attempt(data, plan, index, view.as_mut()))
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().expect("attempt thread")).collect::<Result<Vec<_>>>()
    })?;
    let wall = start.elapsed();

    let peak_rss =
        peak_rss(std::process::id())? + target.helper_pids().into_iter().map(peak_rss).sum::<Result<u64>>()?;
    let disk = disk_usage(scratch)?.saturating_sub(disk_before);
    drop(views);
    drop(base);
    Ok(WorkflowResult { target: target.name(), forks, wall, steps, peak_rss, disk })
}

/// Bytes allocated below `dir`, staying on its filesystem so mounts are not counted.
fn disk_usage(dir: &Path) -> Result<u64> {
    let output = Command::new("du").args(["-s", "-x", "--block-size=1"]).arg(dir).output().context("running du")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let bytes = text.split_whitespace().next().and_then(|n| n.parse().ok());
    bytes.with_context(|| format!("du printed {text:?}: {}", String::from_utf8_lossy(&output.stderr)))
}

/// Start measuring this process's peak memory from now (Linux 4.0 and later).
fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// A process's peak resident memory in bytes, from `VmHWM`.
fn peak_rss(pid: u32) -> Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .context("no VmHWM in /proc status")?;
    Ok(kib * 1024)
}
