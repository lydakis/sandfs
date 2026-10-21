//! A whole task, several times in parallel: what a harness running attempts side by
//! side pays, end to end.
//!
//! Each attempt gets its own fork of one prepared view, then
//! 1. searches every small file for a byte pattern,
//! 2. edits what it found: rewrites matching files, adds notes and deletes a few,
//! 3. checks its work by running `cksum` over part of the tree (on targets that are
//!    real directories; through the Rust API the same checksums are computed
//!    in-process), and compares every checksum with the contents it expects,
//! 4. lists its changes, which is also when the FUSE mount saves them.
//!
//! Forks are made one after another, as a harness would start attempts; the rest runs
//! on one thread per attempt. Besides time, the report shows the peak memory of this
//! process plus any serving processes, and the bytes the attempts left on disk. The
//! caller runs each target in a fresh process, so no target inherits another's memory.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::data::Data;
use crate::sys::{disk_usage, peak_rss, reset_peak_rss};
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
    /// The original bytes of every checked file, to work out the expected checksums.
    originals: Vec<Vec<u8>>,
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
            .collect::<Vec<_>>();
        let originals = checked.iter().map(|path| std::fs::read(data.root().join(path))).collect::<Result<_, _>>()?;
        Ok(Plan { needle, matches, rewrites, deletes, checked, originals })
    }

    /// The checksum and size `cksum` must report for each checked file after `attempt`.
    fn expected_sums(&self, attempt: usize) -> Vec<(u32, u64)> {
        self.checked
            .iter()
            .zip(&self.originals)
            .map(|(path, original)| {
                let mut bytes = original.clone();
                if self.rewrites.contains(path) {
                    bytes.extend_from_slice(stamp(attempt).as_bytes());
                }
                (cksum_crc(&bytes), bytes.len() as u64)
            })
            .collect()
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

/// The line an attempt appends to each file it rewrites.
fn stamp(attempt: usize) -> String {
    format!("\nreviewed by attempt {attempt}\n")
}

/// The table for POSIX `cksum`'s CRC: polynomial 0x04C11DB7, most significant bit first.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// What `cksum` prints for `bytes`: the CRC of the bytes followed by their length
/// (least significant byte first, no trailing zeros), complemented.
fn cksum_crc(bytes: &[u8]) -> u32 {
    let feed = |crc: u32, byte: u8| (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ byte)];
    let mut crc = bytes.iter().fold(0, |crc, byte| feed(crc, *byte));
    let mut len = bytes.len();
    while len > 0 {
        crc = feed(crc, len as u8);
        len >>= 8;
    }
    !crc
}

/// One attempt after its fork: the four timed steps.
fn attempt(
    data: &Data,
    plan: &Plan,
    index: usize,
    expected: &[(u32, u64)],
    view: &mut dyn View,
) -> Result<[Duration; 4]> {
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
        bytes.extend_from_slice(stamp(index).as_bytes());
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
    let sums = match view.root_dir() {
        Some(root) => cksum(root, &plan.checked)?,
        None => plan
            .checked
            .iter()
            .map(|path| view.contents(path).map(|bytes| (cksum_crc(&bytes), bytes.len() as u64)))
            .collect::<Result<Vec<_>>>()?,
    };
    let check = start.elapsed();
    ensure!(sums.len() == expected.len(), "checked {} files, expected {}", sums.len(), expected.len());
    if let Some(i) = (0..sums.len()).find(|&i| sums[i] != expected[i]) {
        bail!("{} has checksum {:?}, expected {:?}", plan.checked[i], sums[i], expected[i]);
    }

    let start = Instant::now();
    let changed = view.changes()?;
    ensure!(changed == plan.changed(), "found {changed} changes, expected {}", plan.changed());
    let changes = start.elapsed();
    Ok([search, edit, check, changes])
}

/// Run `cksum` over `files` below `root` as one process; returns each file's checksum
/// and size, in order.
fn cksum(root: &Path, files: &[String]) -> Result<Vec<(u32, u64)>> {
    let output = Command::new("cksum").current_dir(root).args(files).output().context("running cksum")?;
    if !output.status.success() {
        bail!("cksum failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let crc = fields.next().and_then(|f| f.parse().ok());
            let size = fields.next().and_then(|f| f.parse().ok());
            crc.zip(size).with_context(|| format!("cksum printed {line:?}"))
        })
        .collect()
}

/// Run `attempts` attempts on `target`.
pub fn run(target: &dyn Target, data: &Data, scratch: &Path, attempts: usize) -> Result<WorkflowResult> {
    let plan = Plan::new(data)?;
    let expected: Vec<_> = (0..attempts).map(|index| plan.expected_sums(index)).collect();
    let mut base = target.open(true)?;
    let disk_before = disk_usage(scratch)?;
    reset_peak_rss()?;

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
                let (plan, expected) = (&plan, &expected[index]);
                scope.spawn(move || attempt(data, plan, index, expected, view.as_mut()))
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

#[cfg(test)]
mod tests {
    use super::cksum_crc;

    #[test]
    fn checksums_match_posix_cksum() {
        // Values printed by GNU `cksum` for the same input.
        assert_eq!(cksum_crc(b""), 4_294_967_295);
        assert_eq!(cksum_crc(b"hello\n"), 3_015_617_425);
        assert_eq!(cksum_crc(&[0; 300]), 351_385_237);
    }
}
