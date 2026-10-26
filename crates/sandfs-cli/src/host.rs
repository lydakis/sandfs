//! What differs between the systems the mount runs on: finding mounts, telling whether
//! a process is alive, the mount's options and how to unmount.
//!
//! Linux mounts with the kernel's FUSE through `fusermount3`; macOS mounts this
//! process's NFS server with its own NFS client (see `nfs`). Elsewhere (Windows)
//! nothing is ever mounted, so the lookups find nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The source of the topmost mount at `dir` that could be a workspace's
/// (`sandfs:<name>` for one).
pub fn mount_source(dir: &Path) -> Option<String> {
    mount_table().remove(dir)
}

/// [`mount_source`] for every mount point, read from the system's table once, for
/// callers that look up many: with thousands of workspaces mounted, reading the table
/// once per workspace made finding one cost as much as reading them all.
#[cfg(target_os = "linux")]
pub fn mount_table() -> HashMap<PathBuf, String> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut sources = HashMap::new();
    // Later lines are mounted on top of earlier ones at the same point.
    for line in table.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        // Field 5 is the mount point; the type and source follow the " - " separator.
        let Some(sep) = fields.iter().position(|f| *f == "-") else { continue };
        let (Some(point), Some(fstype), Some(source)) = (fields.get(4), fields.get(sep + 1), fields.get(sep + 2))
        else {
            continue;
        };
        if fstype.starts_with("fuse") {
            sources.insert(PathBuf::from(unescape(point)), unescape(source));
        }
    }
    sources
}

/// [`mount_source`] for every mount point, from `mount`'s table, run once.
#[cfg(target_os = "macos")]
pub fn mount_table() -> HashMap<PathBuf, String> {
    let Ok(output) = std::process::Command::new("/sbin/mount").output() else { return HashMap::new() };
    let table = String::from_utf8_lossy(&output.stdout);
    // Later lines are mounted on top of earlier ones at the same point.
    table.lines().filter_map(parse_mount_line).map(|(source, point)| (PathBuf::from(point), source)).collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn mount_table() -> HashMap<PathBuf, String> {
    HashMap::new()
}

/// A line of macOS `mount` output, `SOURCE on POINT (TYPE, OPTIONS...)`, as its source
/// and mount point. A workspace's NFS source names its export,
/// `localhost:/sandfs/<name>/<secret>`, which reads as `sandfs:<name>` like a FUSE
/// mount's.
#[cfg(any(target_os = "macos", test))]
fn parse_mount_line(line: &str) -> Option<(String, &str)> {
    let (source, rest) = line.split_once(" on ")?;
    let (point, options) = rest.rsplit_once(" (")?;
    let fstype = options.split([',', ')']).next()?;
    let source = match source.strip_prefix("localhost:/sandfs/") {
        Some(export) if fstype == "nfs" => format!("sandfs:{}", export.split('/').next().unwrap_or(export)),
        _ => source.to_owned(),
    };
    Some((source, point))
}

/// Undo mountinfo's octal escapes (`\040` for a space).
#[cfg(any(target_os = "linux", test))]
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let code = std::str::from_utf8(&bytes[i + 1..i + 4]).ok().and_then(|s| u8::from_str_radix(s, 8).ok());
            if let Some(code) = code {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether process `pid` is still running.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else { return false };
    // Signal 0 checks that the process exists without touching it. EPERM means it
    // exists but belongs to someone else.
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

/// Let this process keep as many files open as the system lets it, for the mounts it
/// serves each hold a few (with FUSE: the device and two locks). Most systems start a
/// process with a limit of 1,024 (Linux) or 256 (macOS), which stopped one process at
/// a few hundred mounts.
#[cfg(mount)]
pub fn raise_open_files() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};

    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else { return };
    // macOS refuses more than its per-process maximum, below its unlimited hard limit,
    // so smaller limits are tried until one is taken.
    for wanted in [hard, 1 << 20, 1 << 16, 10_240] {
        if wanted <= soft {
            return;
        }
        if setrlimit(Resource::RLIMIT_NOFILE, wanted, hard).is_ok() {
            return;
        }
    }
}

/// How many more memory maps this process may make, and how many it may have in all
/// (`vm.max_map_count`). Every thread takes four (its stack, its signal stack and
/// their guard pages), and a process that runs out when starting a thread aborts.
///
/// Counting them reads a line per map, 12 ms at 60,000, so while the threads leave
/// plenty to spare this counts each thread as 5 and reads no further.
#[cfg(target_os = "linux")]
pub fn memory_maps_left(spare: usize) -> Option<(usize, usize)> {
    let max: usize = std::fs::read_to_string("/proc/sys/vm/max_map_count").ok()?.trim().parse().ok()?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let threads: usize = status.lines().find_map(|line| line.strip_prefix("Threads:"))?.trim().parse().ok()?;
    if threads * 5 + spare * 2 < max {
        return Some((max - threads * 5, max));
    }
    let used = std::fs::read("/proc/self/maps").ok()?.iter().filter(|&&byte| byte == b'\n').count();
    Some((max.saturating_sub(used), max))
}

/// The options a workspace is mounted with. The source name `sandfs:<name>` is how
/// [`mount_source`] recognizes the workspace's mount later.
#[cfg(fuse)]
pub fn mount_options(name: &str) -> Vec<fuser::MountOption> {
    use fuser::MountOption;

    vec![MountOption::FSName(format!("sandfs:{name}")), MountOption::Subtype("sandfs".to_owned())]
}

/// Who may use a mount besides the user who made it. Root may whenever FUSE lets
/// the mount say so (always for root itself, otherwise only with `user_allow_other`
/// in `/etc/fuse.conf`), so a container run by a root daemon can use the folder. Root
/// can read the sources directly anyway, so this shows it nothing new.
#[cfg(fuse)]
pub fn mount_acl() -> fuser::SessionACL {
    let conf = std::fs::read_to_string("/etc/fuse.conf").unwrap_or_default();
    if nix::unistd::geteuid().is_root() || allows_other(&conf) {
        fuser::SessionACL::RootAndOwner
    } else {
        fuser::SessionACL::Owner
    }
}

/// Whether a `fuse.conf` lets users mount with `allow_other` (or `allow_root`), read
/// as fusermount reads it: comments and surrounding whitespace don't count. Versions
/// of fusermount differ, so a mount refused for asking falls back to owner-only.
#[cfg(any(fuse, test))]
fn allows_other(conf: &str) -> bool {
    conf.lines().any(|line| line.split('#').next().unwrap_or("").trim() == "user_allow_other")
}

/// Commands that unmount a directory, tried in order, and what to do when none exists.
#[cfg(nfs)]
const UNMOUNT: (&[&[&str]], &str) = (&[&["/sbin/umount"]], "/sbin/umount is missing");
#[cfg(fuse)]
const UNMOUNT: (&[&[&str]], &str) =
    (&[&["fusermount3", "-u"], &["fusermount", "-u"]], "fusermount3 is not installed; install fuse3");
#[cfg(mount)]
const FORCED_UNMOUNT: (&[&[&str]], &str) = (&[&["/sbin/umount", "-f"]], "/sbin/umount is missing");

/// Unmount the filesystem at `dir`. With `served_by_nobody` its process is gone: an NFS
/// client would wait for the server to answer, so the unmount is forced. A FUSE mount
/// without its process unmounts like any other.
#[cfg(mount)]
pub fn unmount(dir: &Path, served_by_nobody: bool) -> anyhow::Result<()> {
    let (commands, missing) = if cfg!(nfs) && served_by_nobody { FORCED_UNMOUNT } else { UNMOUNT };
    let ran = commands.iter().find_map(|cmd| std::process::Command::new(cmd[0]).args(&cmd[1..]).arg(dir).status().ok());
    match ran {
        Some(status) if status.success() => Ok(()),
        Some(status) => anyhow::bail!("unmounting {} failed ({status})", dir.display()),
        None => anyhow::bail!("{missing}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuse_conf_allows_other_only_when_uncommented() {
        assert!(allows_other("# comment\nuser_allow_other\n"));
        assert!(allows_other("user_allow_other"));
        assert!(allows_other("  user_allow_other  # for containers\n"));
        assert!(!allows_other("#user_allow_other\n"));
        assert!(!allows_other("mount_max = 1000\n"));
        assert!(!allows_other(""));
    }

    #[test]
    fn mountinfo_escapes_decode() {
        assert_eq!(unescape(r"/home/me/my\040ws"), "/home/me/my ws");
        assert_eq!(unescape(r"/plain"), "/plain");
    }

    #[test]
    fn macos_mount_lines_parse() {
        let line = "localhost:/sandfs/ws/0f3a on /private/tmp/my ws (nfs, nodev, nosuid, mounted by me)";
        assert_eq!(parse_mount_line(line), Some(("sandfs:ws".to_owned(), "/private/tmp/my ws")));
        // Other filesystems count as mounts too, so nothing is mounted over them.
        let other = "localhost:/elsewhere on /Volumes/x (nfs, nodev, nosuid, mounted by me)";
        assert_eq!(parse_mount_line(other), Some(("localhost:/elsewhere".to_owned(), "/Volumes/x")));
        let disk = "/dev/disk3s1 on / (apfs, sealed, local, read-only, journaled)";
        assert_eq!(parse_mount_line(disk), Some(("/dev/disk3s1".to_owned(), "/")));
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
    }
}
