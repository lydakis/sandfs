//! What differs between the systems the mount runs on: finding mounts, telling whether
//! a process is alive, the mount's options and how to unmount.
//!
//! Linux mounts with the kernel's FUSE through `fusermount3`; macOS mounts this
//! process's NFS server with its own NFS client (see `nfs`). Elsewhere (Windows)
//! nothing is ever mounted, so the lookups find nothing.

use std::path::Path;

/// The source of the topmost FUSE mount at `dir` (`sandfs:<name>` for a workspace).
#[cfg(target_os = "linux")]
pub fn mount_source(dir: &Path) -> Option<String> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    table.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        // Field 5 is the mount point; the type and source follow the " - " separator.
        let sep = fields.iter().position(|f| *f == "-")?;
        let (fstype, source) = (fields.get(sep + 1)?, fields.get(sep + 2)?);
        (fields.get(4).is_some_and(|point| Path::new(&unescape(point)) == dir) && fstype.starts_with("fuse"))
            .then(|| unescape(source))
    })
}

/// The source of the topmost mount at `dir`, from `mount`'s table (`sandfs:<name>` for
/// a workspace).
#[cfg(target_os = "macos")]
pub fn mount_source(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("/sbin/mount").output().ok()?;
    let table = String::from_utf8_lossy(&output.stdout);
    table
        .lines()
        .rev()
        .find_map(|line| parse_mount_line(line).filter(|(_, point)| Path::new(point) == dir).map(|(source, _)| source))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn mount_source(_dir: &Path) -> Option<String> {
    None
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
