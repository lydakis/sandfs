//! Compare two runs saved with `--json`: every number of the second run against the
//! same number of the first, and against what it has to beat in its own run (the host
//! folder, overlayfs, a clone), so a report says both whether something moved and how
//! far it is from its reference.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

/// One measured number: its samples (nanoseconds, or bytes for memory and disk), and
/// the key of the row in the same run it is compared with, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub samples: Vec<u64>,
    pub unit: Unit,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Nanos,
    Bytes,
}

/// Every number in a `--json` report, by a key naming its table, row and column.
pub fn rows(report: &Value) -> Result<BTreeMap<String, Row>> {
    let mut rows = BTreeMap::new();
    let mut add = |key: String, samples: Vec<u64>, unit: Unit, reference: Option<String>| {
        if !samples.is_empty() {
            rows.insert(key, Row { samples, unit, reference });
        }
    };
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let numbers = |value: &Value| -> Vec<u64> { list(value).iter().filter_map(Value::as_u64).collect() };
    let text = |value: &Value, key: &str| value[key].as_str().map(str::to_owned).with_context(|| format!("no {key}"));

    // The basic workloads and everyday operations: one row per column, against the
    // host column of the same row.
    for workload in list(&report["workloads"]) {
        let name = text(&workload, "workload")?;
        for (column, samples) in workload["nanos"].as_object().into_iter().flatten() {
            let reference = (column != "host").then(|| format!("{name} | host"));
            add(format!("{name} | {column}"), numbers(samples), Unit::Nanos, reference);
        }
    }
    for group in list(&report["everyday"]) {
        let source = text(&group, "source")?;
        for row in list(&group["rows"]) {
            let operation = text(&row, "operation")?;
            let cells = row["nanos"].as_object().cloned().unwrap_or_default();
            let host = cells.contains_key("host");
            for (column, samples) in &cells {
                let reference = (host && column != "host").then(|| format!("everyday {source} / {operation} | host"));
                add(format!("everyday {source} / {operation} | {column}"), numbers(samples), Unit::Nanos, reference);
            }
        }
    }
    // A slow source read lazily, against fetching everything first.
    for row in list(&report["lazy"]) {
        let name = text(&row, "workload")?;
        for way in ["lazy", "eager"] {
            let samples = list(&row[way]).iter().filter_map(|run| run["nanos"].as_u64()).collect();
            let reference = (way == "lazy").then(|| format!("{name} | eager"));
            add(format!("{name} | {way}"), samples, Unit::Nanos, reference);
        }
    }
    // Parallel attempts: time, memory and disk per target, against overlayfs where it
    // ran, else the host's full copies.
    let flows = list(&report["workflow"]);
    let targets: Vec<String> = flows.iter().filter_map(|run| run["target"].as_str().map(str::to_owned)).collect();
    let floor = ["overlayfs", "host"].into_iter().find(|name| targets.iter().any(|target| target == name));
    let mut by_target: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for run in &flows {
        by_target.entry(text(run, "target")?).or_default().push(run);
    }
    for (target, runs) in &by_target {
        for (field, label, unit) in [
            ("wall_nanos", "wall", Unit::Nanos),
            ("forks_nanos", "forks", Unit::Nanos),
            ("peak_rss_bytes", "peak memory", Unit::Bytes),
            ("disk_bytes", "disk", Unit::Bytes),
        ] {
            let samples = runs.iter().filter_map(|run| run[field].as_u64()).collect();
            let reference = floor.filter(|floor| floor != target).map(|floor| format!("workflow {label} | {floor}"));
            add(format!("workflow {label} | {target}"), samples, unit, reference);
        }
    }
    // A remote repository, against a blobless clone.
    for row in list(&report["remote"]) {
        let method = text(&row, "method")?;
        for (field, label) in [
            ("ready_nanos", "ready"),
            ("first_nanos", "first read"),
            ("samples_nanos", "scattered reads"),
            ("search_nanos", "folder search"),
            ("fork_nanos", "second copy"),
            ("status_nanos", "first git status"),
            ("commit_nanos", "commit"),
            ("status_again_nanos", "git status again"),
            ("status_fork_nanos", "git status in the second copy"),
            ("find_nanos", "find"),
            ("grep_nanos", "grep -r"),
            ("grep_again_nanos", "grep -r again"),
            ("remove_nanos", "remove the second copy"),
        ] {
            let samples = row[field].as_u64().into_iter().collect();
            let reference = (method != "blobless").then(|| format!("remote {label} | blobless"));
            add(format!("remote {label} | {method}"), samples, Unit::Nanos, reference);
        }
        let samples = row["peak_rss_bytes"].as_u64().into_iter().collect();
        add(format!("remote peak memory | {method}"), samples, Unit::Bytes, None);
    }
    // A build or command, against the host folder.
    if let Some(build) = report["build"].as_object() {
        for column in list(&build["columns"]) {
            let name = text(&column, "column")?;
            let reference = |label: &str| (name != "host folder").then(|| format!("build {label} | host folder"));
            add(
                format!("build first run | {name}"),
                column["cold_nanos"].as_u64().into_iter().collect(),
                Unit::Nanos,
                reference("first run"),
            );
            add(
                format!("build run again | {name}"),
                numbers(&column["rebuild_nanos"]),
                Unit::Nanos,
                reference("run again"),
            );
            let rss = column["server_rss"].as_u64().into_iter().collect();
            add(format!("build server memory | {name}"), rss, Unit::Bytes, None);
        }
    }
    Ok(rows)
}

/// How a row moved between two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Slower,
    Faster,
    Same,
}

/// Slower or faster only when the medians differ by more than `threshold` (0.25 is
/// 25%) and, for rows with several samples, every sample of one run is beyond every
/// sample of the other: shared machines swing from run to run, and a change that
/// overlaps the old spread is more likely noise than a regression.
pub fn verdict(base: &Row, head: &Row, threshold: f64) -> Verdict {
    let ratio = median(&head.samples) / median(&base.samples).max(1.0);
    let (base_min, base_max) = bounds(&base.samples);
    let (head_min, head_max) = bounds(&head.samples);
    let several = base.samples.len() > 1 && head.samples.len() > 1;
    if ratio > 1.0 + threshold && (!several || head_min > base_max) {
        Verdict::Slower
    } else if ratio < 1.0 / (1.0 + threshold) && (!several || head_max < base_min) {
        Verdict::Faster
    } else {
        Verdict::Same
    }
}

fn bounds(samples: &[u64]) -> (u64, u64) {
    (samples.iter().copied().min().unwrap_or(0), samples.iter().copied().max().unwrap_or(0))
}

fn median(samples: &[u64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.is_empty() {
        0.0
    } else if sorted.len() % 2 == 0 {
        (sorted[mid - 1] as f64 + sorted[mid] as f64) / 2.0
    } else {
        sorted[mid] as f64
    }
}

fn show(row: &Row) -> String {
    let value = median(&row.samples);
    match row.unit {
        Unit::Nanos => crate::fmt_duration(Duration::from_nanos(value as u64)),
        Unit::Bytes => crate::fmt_bytes(value as u64),
    }
}

/// Several runs of one side pooled: each number's samples from all of them.
pub fn pool(runs: Vec<BTreeMap<String, Row>>) -> BTreeMap<String, Row> {
    let mut pooled: BTreeMap<String, Row> = BTreeMap::new();
    for run in runs {
        for (key, row) in run {
            match pooled.get_mut(&key) {
                Some(seen) => seen.samples.extend(row.samples),
                None => {
                    pooled.insert(key, row);
                }
            }
        }
    }
    pooled
}

/// Print the comparison as a Markdown table, the moved rows first, and return how many
/// rows got slower. The references themselves (the host folder, overlayfs, a clone)
/// are left out: they don't change with sandfs, so a move there is the machine's.
pub fn print(base: &BTreeMap<String, Row>, head: &BTreeMap<String, Row>, threshold: f64, all: bool) -> usize {
    let references: std::collections::BTreeSet<&String> =
        head.values().filter_map(|row| row.reference.as_ref()).collect();
    let mut lines = Vec::new();
    let mut slower = 0;
    let mut compared = 0;
    for (key, now) in head {
        let Some(before) = base.get(key) else { continue };
        if references.contains(key) {
            continue;
        }
        compared += 1;
        let verdict = verdict(before, now, threshold);
        if verdict == Verdict::Slower {
            slower += 1;
        }
        if verdict == Verdict::Same && !all {
            continue;
        }
        let change = median(&now.samples) / median(&before.samples).max(1.0);
        let against = now
            .reference
            .as_ref()
            .and_then(|reference| head.get(reference).map(|floor| (reference, floor)))
            .map(|(reference, floor)| {
                let column = reference.rsplit(" | ").next().unwrap_or(reference);
                format!("{:.1}x {column}", median(&now.samples) / median(&floor.samples).max(1.0))
            })
            .unwrap_or_default();
        let mark = match verdict {
            Verdict::Slower => "slower",
            Verdict::Faster => "faster",
            Verdict::Same => "",
        };
        // Keys name the number, then its column after " | ", which is a table cell too.
        lines
            .push((verdict, format!("| {key} | {} | {} | {change:.2}x {mark} | {against} |", show(before), show(now))));
    }
    lines.sort_by_key(|(verdict, _)| match verdict {
        Verdict::Slower => 0,
        Verdict::Faster => 1,
        Verdict::Same => 2,
    });
    let moved = lines.iter().filter(|(verdict, _)| *verdict != Verdict::Same).count();
    println!(
        "{moved} of {} numbers moved by more than {:.0}% beyond both runs' spread; {slower} got slower.",
        compared,
        threshold * 100.0
    );
    if !lines.is_empty() {
        println!();
        println!("| number | column | before | after | after / before | after against its reference |");
        println!("|---|---|---:|---:|---|---|");
        for (_, line) in &lines {
            println!("{line}");
        }
    }
    slower
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(samples: &[u64]) -> Row {
        Row { samples: samples.to_vec(), unit: Unit::Nanos, reference: None }
    }

    #[test]
    fn only_changes_beyond_the_threshold_and_the_spread_count() {
        assert_eq!(verdict(&row(&[100, 105, 110]), &row(&[200, 210, 220]), 0.25), Verdict::Slower);
        assert_eq!(verdict(&row(&[100, 105, 110]), &row(&[50, 52, 55]), 0.25), Verdict::Faster);
        // Twice as slow in the median, but one sample overlaps the old spread.
        assert_eq!(verdict(&row(&[100, 105, 300]), &row(&[200, 210, 220]), 0.25), Verdict::Same);
        // Beyond the spread but within the threshold.
        assert_eq!(verdict(&row(&[100, 101, 102]), &row(&[110, 111, 112]), 0.25), Verdict::Same);
        // A single sample has no spread, so the threshold alone decides.
        assert_eq!(verdict(&row(&[100]), &row(&[130]), 0.25), Verdict::Slower);
    }

    #[test]
    fn runs_of_one_side_pool_their_samples() {
        let one = BTreeMap::from([("a | x".to_owned(), row(&[1, 2]))]);
        let two = BTreeMap::from([("a | x".to_owned(), row(&[3])), ("b | x".to_owned(), row(&[4]))]);
        let pooled = pool(vec![one, two]);
        assert_eq!(pooled["a | x"].samples, [1, 2, 3]);
        assert_eq!(pooled["b | x"].samples, [4]);
    }

    #[test]
    fn every_table_becomes_rows_with_their_references() {
        let report = serde_json::json!({
            "workloads": [{"workload": "small/walk", "what": "", "nanos": {"host": [3, 3], "sandfs FUSE": [30, 31]}}],
            "everyday": [{"source": "a folder", "rows": [{"operation": "ls", "what": "", "nanos": {"host": [1], "sandfs": [2]}}]}],
            "lazy": [{"workload": "lazy/read-1%", "what": "", "lazy": [{"nanos": 5, "calls": 1}], "eager": [{"nanos": 50, "calls": 9}]}],
            "workflow": [
                {"target": "overlayfs", "wall_nanos": 10, "forks_nanos": 1, "peak_rss_bytes": 7, "disk_bytes": 1},
                {"target": "sandfs FUSE", "wall_nanos": 20, "forks_nanos": 2, "peak_rss_bytes": 70, "disk_bytes": 2}
            ],
            "remote": [{"method": "sandfs", "ready_nanos": 1, "grep_nanos": null, "peak_rss_bytes": 9}],
            "build": {"job": "", "parallel": 1, "columns": [
                {"column": "host folder", "cold_nanos": 10, "rebuild_nanos": [1, 2], "server_rss": null},
                {"column": "sandfs mount", "cold_nanos": 12, "rebuild_nanos": [3, 4], "server_rss": 5}
            ]}
        });
        let rows = rows(&report).unwrap();
        let reference = |key: &str| rows[key].reference.clone();
        assert_eq!(rows["small/walk | sandfs FUSE"].samples, [30, 31]);
        assert_eq!(reference("small/walk | sandfs FUSE").as_deref(), Some("small/walk | host"));
        assert_eq!(reference("small/walk | host"), None);
        assert_eq!(reference("everyday a folder / ls | sandfs").as_deref(), Some("everyday a folder / ls | host"));
        assert_eq!(reference("lazy/read-1% | lazy").as_deref(), Some("lazy/read-1% | eager"));
        assert_eq!(
            reference("workflow peak memory | sandfs FUSE").as_deref(),
            Some("workflow peak memory | overlayfs")
        );
        assert_eq!(rows["workflow peak memory | sandfs FUSE"].unit, Unit::Bytes);
        assert_eq!(reference("remote ready | sandfs").as_deref(), Some("remote ready | blobless"));
        assert!(!rows.contains_key("remote grep -r | sandfs"), "a step that didn't run has no row");
        assert_eq!(reference("build run again | sandfs mount").as_deref(), Some("build run again | host folder"));
        assert_eq!(rows["build server memory | sandfs mount"].samples, [5]);
        assert!(!rows.contains_key("build server memory | host folder"));
    }
}
