//! What the benchmark asks the operating system: how much memory a process holds, which
//! processes run below another, how much disk a folder takes, and how to force an
//! unmount. Linux answers from `/proc`; macOS from `getrusage`, `ps` and `vmmap`.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// A process's resident memory now, in bytes.
pub fn current_rss(pid: u32) -> Result<u64> {
    #[cfg(target_os = "linux")]
    return proc_status_kib(pid, "VmRSS:");
    #[cfg(target_os = "macos")]
    return ps_rss(pid);
}

/// A process's peak memory in bytes: its peak resident memory. macOS reports that only
/// to the process itself, so for another process it is the peak physical footprint
/// `vmmap` shows, the memory Activity Monitor counts, compressed pages included.
pub fn peak_rss(pid: u32) -> Result<u64> {
    #[cfg(target_os = "linux")]
    return proc_status_kib(pid, "VmHWM:");
    #[cfg(target_os = "macos")]
    if pid == std::process::id() {
        // Bytes on macOS.
        Ok(nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF)?.max_rss() as u64)
    } else {
        vmmap_peak(pid)
    }
}

/// Start measuring this process's peak memory from now. Linux 4.0 and later can; on
/// macOS the peak covers the process's whole life, which is why each workflow runs in
/// a fresh process.
pub fn reset_peak_rss() -> Result<()> {
    #[cfg(target_os = "linux")]
    std::fs::write("/proc/self/clear_refs", "5").context("resetting the peak memory count")?;
    Ok(())
}

/// The peak memory of `pid` and of every process still running below it, in bytes.
/// Helpers that already exited aren't counted: their peaks went to `pid`'s own child
/// usage, which only `pid` can read.
pub fn tree_peak_rss(pid: u32) -> Result<u64> {
    let parents = parents()?;
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(parents.iter().filter(|&&(_, p)| p == parent).map(|&(child, _)| child));
        i += 1;
    }
    let mut total = peak_rss(pid)?;
    for &process in &tree[1..] {
        // A helper may exit before its memory is read.
        total += peak_rss(process).unwrap_or(0);
    }
    Ok(total)
}

/// Bytes allocated below `dir`, staying on its filesystem so mounts are not counted.
pub fn disk_usage(dir: &Path) -> Result<u64> {
    let output = Command::new("du").args(["-s", "-x", "-k"]).arg(dir).output().context("running du")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let kib = text.split_whitespace().next().and_then(|n| n.parse::<u64>().ok());
    kib.map(|kib| kib * 1024)
        .with_context(|| format!("du printed {text:?}: {}", String::from_utf8_lossy(&output.stderr)))
}

/// Detach the mount at `dir` after `sandfs unmount` failed, so a broken run leaves no
/// mount behind.
pub fn force_unmount(dir: &Path) {
    #[cfg(target_os = "linux")]
    let mut command = Command::new("fusermount3");
    #[cfg(target_os = "linux")]
    command.arg("-uz");
    #[cfg(target_os = "macos")]
    let mut command = Command::new("umount");
    #[cfg(target_os = "macos")]
    command.arg("-f");
    let _ = command.arg(dir).stderr(Stdio::null()).status();
}

/// Every running process with its parent.
fn parents() -> Result<Vec<(u32, u32)>> {
    let mut parents = Vec::new();
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir("/proc")? {
        let Ok(child) = entry?.file_name().to_string_lossy().parse::<u32>() else { continue };
        // A process may exit between the listing and the read.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else { continue };
        // The parent is the second field after the command name, which may hold spaces
        // and parentheses itself, so split after its last closing parenthesis.
        let parent = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok());
        if let Some(parent) = parent {
            parents.push((child, parent));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output().context("running ps")?;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            if let (Some(Ok(child)), Some(Ok(parent))) = (fields.next(), fields.next()) {
                parents.push((child, parent));
            }
        }
    }
    Ok(parents)
}

#[cfg(target_os = "linux")]
fn proc_status_kib(pid: u32, field: &str) -> Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .with_context(|| format!("no {field} in /proc status"))?;
    Ok(kib * 1024)
}

#[cfg(target_os = "macos")]
fn ps_rss(pid: u32) -> Result<u64> {
    let output = Command::new("ps").args(["-o", "rss=", "-p"]).arg(pid.to_string()).output().context("running ps")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let kib = text.trim().parse::<u64>().with_context(|| format!("ps printed {text:?} for process {pid}"))?;
    Ok(kib * 1024)
}

/// The peak physical footprint `vmmap` reports for another process.
#[cfg(target_os = "macos")]
fn vmmap_peak(pid: u32) -> Result<u64> {
    let output = Command::new("vmmap").arg("-summary").arg(pid.to_string()).stderr(Stdio::null()).output();
    let text = String::from_utf8_lossy(&output.context("running vmmap")?.stdout).into_owned();
    let size = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Physical footprint (peak):"))
        .with_context(|| format!("vmmap printed no peak footprint for process {pid}"))?;
    parse_size(size.trim()).with_context(|| format!("vmmap printed a peak footprint of {size:?}"))
}

/// A size as `vmmap` prints it, such as `812K` or `1.5G`.
#[cfg(any(target_os = "macos", test))]
fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(text.len()));
    let scale = match unit {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number.parse::<f64>().ok()? * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::{current_rss, disk_usage, parse_size, peak_rss, tree_peak_rss};

    #[test]
    fn sizes_parse_as_vmmap_prints_them() {
        assert_eq!(parse_size("812K"), Some(812 * 1024));
        assert_eq!(parse_size("1.5M"), Some(3 << 19));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("12"), Some(12));
        assert_eq!(parse_size("3X"), None);
    }

    #[test]
    fn a_process_tree_counts_its_running_children() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        // Until the child has become `sleep`, it is a copy of this process with its peak.
        let name = || {
            let output =
                std::process::Command::new("ps").args(["-o", "comm=", "-p"]).arg(child.id().to_string()).output();
            String::from_utf8_lossy(&output.unwrap().stdout).trim().to_owned()
        };
        while !name().ends_with("sleep") {
            std::thread::yield_now();
        }
        // Peaks only grow, so reading the parts before the whole keeps the sum below it.
        let alone = peak_rss(std::process::id()).unwrap();
        let sleeping = peak_rss(child.id()).unwrap();
        let tree = tree_peak_rss(std::process::id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(sleeping > 0);
        assert!(tree >= alone + sleeping, "tree {tree} < {alone} + {sleeping}");
    }

    #[test]
    fn memory_and_disk_read_back() {
        assert!(current_rss(std::process::id()).unwrap() > 0);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), vec![1; 1 << 20]).unwrap();
        assert!(disk_usage(dir.path()).unwrap() >= 1 << 20);
    }
}
