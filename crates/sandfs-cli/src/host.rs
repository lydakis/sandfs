//! What differs between the systems the mount runs on: finding mounts, telling whether
//! a process is alive, the mount's options and how to unmount.
//!
//! Linux mounts with the kernel's FUSE through `fusermount3`. Elsewhere (macOS,
//! Windows) nothing is mounted yet, so the lookups find nothing.

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

#[cfg(not(target_os = "linux"))]
pub fn mount_source(_dir: &Path) -> Option<String> {
    None
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

/// Unmount the FUSE filesystem at `dir`.
#[cfg(fuse)]
pub fn unmount(dir: &Path) -> anyhow::Result<()> {
    let ran = [&["fusermount3", "-u"], &["fusermount", "-u"]]
        .iter()
        .find_map(|cmd| std::process::Command::new(cmd[0]).args(&cmd[1..]).arg(dir).status().ok());
    match ran {
        Some(status) if status.success() => Ok(()),
        Some(status) => anyhow::bail!("unmounting {} failed ({status})", dir.display()),
        None => anyhow::bail!("fusermount3 is not installed; install fuse3"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_escapes_decode() {
        assert_eq!(unescape(r"/home/me/my\040ws"), "/home/me/my ws");
        assert_eq!(unescape(r"/plain"), "/plain");
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
    }
}
