//! `sandfs shell`: run a shell, or one command, that sees a mounted workspace and
//! nothing else of the machine's files.
//!
//! Sandfs enforces nothing here; the operating system does. On Linux the program runs
//! under bubblewrap in new namespaces, with the system's programs read-only and the
//! mount and a private temporary folder writable. On macOS it runs under the built-in
//! sandbox (`sandbox-exec`) with a profile allowing the same. Either way the network
//! is off unless asked for, the environment holds only a few harmless variables, and
//! HOME is a fresh folder outside the mount, so programs' caches stay out of the
//! workspace's changes.
//!
//! The program gets a session of its own, without the caller's terminal as its
//! controlling terminal, so it can't push keystrokes into the caller's shell
//! (`TIOCSTI`) for that shell to run once the program exits. Linux kernels that refuse
//! `TIOCSTI` everywhere let an interactive shell keep its terminal, and job control.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail};

/// What the confined program gets besides the mount.
pub struct Confined {
    /// The mounted folder: readable, writable, and the working directory.
    pub dir: PathBuf,
    /// The workspace's source on this machine, hidden from the program even where it
    /// lies in a folder the program may read, such as below `/usr`.
    pub source: Option<PathBuf>,
    /// Folders of this machine the program may read, such as the object store that a
    /// git workspace's `.git` points to, shown even where they lie in the source.
    pub readable: Vec<PathBuf>,
    /// Let the program reach the network.
    pub network: bool,
    /// The program and its arguments; the user's shell when empty.
    pub command: Vec<OsString>,
}

/// Variables passed through from the caller's environment. Anything else, tokens and
/// keys among them, stays out.
const PASSED_ENV: &[&str] = &["PATH", "TERM", "COLORTERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "LOGNAME", "SHELL"];

/// Passed through too when the program may use the network, so it goes the same way.
const PROXY_ENV: &[&str] =
    &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy"];

/// Run the program confined and return its exit code.
pub fn run(confined: &Confined) -> Result<i32> {
    let dir = confined.dir.canonicalize().with_context(|| format!("{}", confined.dir.display()))?;
    let source = confined.source.as_deref().and_then(|source| source.canonicalize().ok());
    let readable: Vec<PathBuf> = confined.readable.iter().filter_map(|path| path.canonicalize().ok()).collect();
    // HOME and the program's temporary files belong outside the workspace, and nothing
    // may be written into its source or the folders it borrows.
    let tmp_root = std::env::temp_dir().canonicalize()?;
    for inside in [&dir].into_iter().chain(&source).chain(&readable) {
        if tmp_root.starts_with(inside) {
            bail!(
                "the temporary folder {} is inside {}; set TMPDIR to another folder",
                tmp_root.display(),
                inside.display()
            );
        }
    }
    let tmp = tempfile::Builder::new().prefix("sandfs-shell-").tempdir_in(&tmp_root)?;
    let tmp_dir = tmp.path().to_path_buf();
    let home = tmp_dir.join("home");
    std::fs::create_dir(&home)?;

    let mut command = confined.command.clone();
    if command.is_empty() {
        command.push(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()));
    }
    let places = Places { dir: &dir, tmp: &tmp_dir, source: source.as_deref(), readable: &readable };
    let mut cmd = confining(&places, confined.network, &command)?;
    cmd.current_dir(&dir).env_clear();
    for name in PASSED_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    if confined.network {
        for name in PROXY_ENV {
            match std::env::var_os(name) {
                Some(value) if holds_password(&value) => {
                    eprintln!("sandfs shell: leaving out {name}, which holds a user name or password");
                }
                Some(value) => {
                    cmd.env(name, value);
                }
                None => {}
            }
        }
    }
    cmd.env("HOME", &home).env("TMPDIR", &tmp_dir);
    // A git workspace's file monitor is `sandfs git-fsmonitor`, which reads the
    // workspace's state outside the sandbox, so git looks at the files itself.
    cmd.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "core.fsmonitor").env("GIT_CONFIG_VALUE_0", "false");
    let child = cmd.spawn().with_context(|| format!("starting {}", cmd.get_program().to_string_lossy()))?;
    #[cfg(target_os = "macos")]
    let group = nix::unistd::Pid::from_raw(i32::try_from(child.id())?);
    let status = wait(child)?;
    // What the program left running in its session goes with it, as bubblewrap's
    // namespace takes everything in it down on Linux.
    #[cfg(target_os = "macos")]
    let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
    // A program killed by a signal exits as a shell reports it, 128 + the signal.
    #[cfg(unix)]
    let signalled = std::os::unix::process::ExitStatusExt::signal(&status).map(|signal| 128 + signal);
    #[cfg(not(unix))]
    let signalled = None;
    Ok(status.code().or(signalled).unwrap_or(1))
}

/// Whether a proxy address carries credentials, as in `http://user:pass@proxy:3128`.
fn holds_password(value: &std::ffi::OsStr) -> bool {
    let value = value.to_string_lossy();
    let rest = value.split_once("://").map_or(&*value, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().is_some_and(|authority| authority.contains('@'))
}

/// The folders a confined program is given or kept from.
struct Places<'a> {
    dir: &'a Path,
    tmp: &'a Path,
    source: Option<&'a Path>,
    readable: &'a [PathBuf],
}

/// Whether the program needs a session of its own to keep it from the caller's
/// terminal: always, unless it is a terminal on a kernel that refuses `TIOCSTI`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn new_session() -> bool {
    let tiocsti_refused = std::fs::read_to_string("/proc/sys/dev/tty/legacy_tiocsti").is_ok_and(|on| on.trim() == "0");
    !(tiocsti_refused && std::io::IsTerminal::is_terminal(&std::io::stdin()))
}

/// What the Linux sandbox shows of the machine, read-only: the system's programs and
/// libraries, and what programs read from /etc to start, find users, check
/// certificates and look up names. The rest of /etc can describe the machine and its
/// accounts, and certificate folders can hold private keys, so only these.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_ETC: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/profile",
    "/etc/profile.d",
    "/etc/bash.bashrc",
    "/etc/inputrc",
    "/etc/zsh",
    "/etc/terminfo",
    "/etc/mime.types",
    "/etc/gitconfig",
    "/etc/ssl/certs",
    "/etc/ssl/openssl.cnf",
    "/etc/ca-certificates",
    "/etc/ca-certificates.conf",
    "/etc/pki/ca-trust/extracted",
    "/etc/pki/tls/certs",
    "/etc/pki/tls/cert.pem",
    "/etc/pki/tls/openssl.cnf",
];

/// Read only with --network, to look up names.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_ETC_NETWORK: &[&str] = &["/etc/resolv.conf", "/etc/hosts", "/etc/host.conf", "/etc/gai.conf"];

#[cfg(target_os = "linux")]
fn confining(places: &Places, network: bool, command: &[OsString]) -> Result<Command> {
    // Only where the system installs it: PATH could name a program the workspace wrote.
    let bwrap = ["/usr/bin", "/bin", "/usr/local/bin", "/run/current-system/sw/bin"]
        .map(|dir| Path::new(dir).join("bwrap"))
        .into_iter()
        .find(|path| path.is_file())
        .context("sandfs shell needs bubblewrap: install the bubblewrap package")?;
    let mut cmd = Command::new(bwrap);
    cmd.args(["--unshare-all", "--die-with-parent"]);
    if network {
        cmd.arg("--share-net");
    }
    if new_session() {
        cmd.arg("--new-session");
    }
    let mut shown = vec!["/usr"];
    cmd.args(["--ro-bind", "/usr", "/usr"]);
    // Merged-/usr systems link these into /usr; older ones keep real folders.
    for top in ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"] {
        match std::fs::read_link(top) {
            Ok(target) => {
                cmd.arg("--symlink").arg(target).arg(top);
            }
            Err(_) => {
                cmd.args(["--ro-bind-try", top, top]);
                shown.push(top);
            }
        }
    }
    let etc = LINUX_ETC.iter().chain(if network { LINUX_ETC_NETWORK } else { &[] });
    for &path in etc {
        cmd.args(["--ro-bind-try", path, path]);
        shown.push(path);
    }
    // A source inside what is shown would let the program read it around the
    // workspace, so it is covered; one holding what is shown can't be.
    if let Some(source) = places.source {
        if let Some(held) = shown.iter().find(|shown| Path::new(shown).starts_with(source)) {
            bail!("sandfs shell can't hide the source {} from the shell: it holds {held}", source.display());
        }
        if shown.iter().any(|shown| source.starts_with(shown)) {
            cmd.arg("--tmpfs").arg(source);
        }
    }
    cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    for path in places.readable {
        cmd.arg("--ro-bind").arg(path).arg(path);
    }
    cmd.arg("--bind").arg(places.dir).arg(places.dir);
    cmd.arg("--bind").arg(places.tmp).arg(places.tmp);
    cmd.arg("--chdir").arg(places.dir).arg("--").args(command);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn confining(places: &Places, network: bool, command: &[OsString]) -> Result<Command> {
    let profile = places.tmp.join("confine.sb");
    std::fs::write(&profile, macos_profile(places, network)?)?;
    // `sandfs shell-exec` starts a new session, then becomes sandbox-exec.
    let mut cmd = crate::relaunch::command()?;
    cmd.args(["shell-exec", "--", "/usr/bin/sandbox-exec", "-f"]).arg(profile).args(command);
    Ok(cmd)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn confining(_: &Places, _: bool, _: &[OsString]) -> Result<Command> {
    bail!("sandfs shell runs on Linux and macOS")
}

/// Become the leader of a new session, then run `command` in place of this process:
/// `sandfs shell-exec`, which `sandfs shell` runs on macOS, where nothing else starts a
/// program in a session of its own.
#[cfg(unix)]
pub fn exec_in_new_session(command: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let Some((program, args)) = command.split_first() else { bail!("nothing to run") };
    nix::unistd::setsid()?;
    let err = Command::new(program).args(args).exec();
    Err(err).with_context(|| format!("starting {}", program.to_string_lossy()))
}

#[cfg(not(unix))]
pub fn exec_in_new_session(_: &[OsString]) -> Result<()> {
    bail!("sandfs shell runs on Linux and macOS")
}

/// Wait for the program. On Linux bubblewrap stays in the caller's process group, so
/// the terminal's signals reach it, and it takes the program down when this process
/// goes. On macOS the program's session is out of their reach, so the terminal's
/// signals are passed on to it: those that end a program, a resize, and a stop, which
/// stops this process too until it is continued.
#[cfg(not(all(target_os = "macos", mount)))]
fn wait(mut child: std::process::Child) -> Result<ExitStatus> {
    Ok(child.wait()?)
}

#[cfg(all(target_os = "macos", mount))]
fn wait(mut child: std::process::Child) -> Result<ExitStatus> {
    use nix::sys::signal::{Signal, killpg};
    use tokio::signal::unix::{SignalKind, signal};
    // The session leader's process group has its process id.
    let group = nix::unistd::Pid::from_raw(i32::try_from(child.id())?);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut quit = signal(SignalKind::quit())?;
        let mut resize = signal(SignalKind::window_change())?;
        let mut stop = signal(SignalKind::from_raw(Signal::SIGTSTP as i32))?;
        let mut exited = tokio::task::spawn_blocking(move || child.wait());
        loop {
            let passed = tokio::select! {
                status = &mut exited => return Ok(status??),
                _ = interrupt.recv() => Signal::SIGINT,
                _ = terminate.recv() => Signal::SIGTERM,
                _ = hangup.recv() => Signal::SIGHUP,
                _ = quit.recv() => Signal::SIGQUIT,
                _ = resize.recv() => Signal::SIGWINCH,
                _ = stop.recv() => {
                    let _ = killpg(group, Signal::SIGSTOP);
                    let _ = nix::sys::signal::raise(Signal::SIGSTOP);
                    Signal::SIGCONT
                }
            };
            let _ = killpg(group, passed);
        }
    })
}

/// The sandbox profile for macOS. Programs may read the system's software and write
/// only the mount and the temporary folder; every folder above those two can be looked
/// at but not listed, and the source can't be read even where the system's software is.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_profile(places: &Places, network: bool) -> Result<String> {
    let mut above: Vec<String> = Vec::new();
    for path in [places.dir, places.tmp].into_iter().chain(places.readable.iter().map(PathBuf::as_path)) {
        for ancestor in path.ancestors().skip(1) {
            above.push(quoted(ancestor)?);
        }
    }
    above.sort();
    above.dedup();
    let above: String = above.iter().map(|path| format!("\n  (literal {path})")).collect();
    let hidden = match places.source {
        Some(source) => format!("(deny file-read* file-write* (subpath {}))", quoted(source)?),
        None => String::new(),
    };
    let mut readable = String::new();
    for path in places.readable {
        readable.push_str(&format!("(allow file-read* (subpath {}))\n", quoted(path)?));
    }
    let (dir, tmp) = (quoted(places.dir)?, quoted(places.tmp)?);
    let network = if network { NETWORK_ON } else { "(deny network*)" };
    Ok(fill(
        MACOS_PROFILE,
        &[
            ("@ABOVE@", &above),
            ("@HIDDEN@", &hidden),
            ("@READABLE@", &readable),
            ("@DIR@", &dir),
            ("@TMP@", &tmp),
            ("@NETWORK@", network),
        ],
    ))
}

/// `template` with each placeholder replaced in one pass, so a value is never read as
/// a placeholder itself.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut filled = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('@') {
        filled.push_str(&rest[..at]);
        rest = &rest[at..];
        match values.iter().find(|(key, _)| rest.starts_with(key)) {
            Some((key, value)) => {
                filled.push_str(value);
                rest = &rest[key.len()..];
            }
            None => {
                filled.push('@');
                rest = &rest[1..];
            }
        }
    }
    filled.push_str(rest);
    filled
}

/// `path` as a profile string.
fn quoted(path: &Path) -> Result<String> {
    let Some(path) = path.to_str() else { bail!("{} is not valid UTF-8", path.display()) };
    if path.contains(['"', '\\', '\n']) {
        bail!("sandfs shell can't confine to {path}: rename it without quotes, backslashes or newlines");
    }
    Ok(format!("\"{path}\""))
}

/// Found by trial on macOS 27: what a shell, git, python3 and the developer tools need
/// to start, with everything else denied. A later rule wins over an earlier one.
const MACOS_PROFILE: &str = r#"(version 1)
(deny default)

(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
;; Except other processes' arguments and environments, sandfs's own among them: the
;; caller's tokens are in it.
(deny sysctl-read (sysctl-name-prefix "kern.procargs"))
(allow user-preference-read
  (preference-domain "kCFPreferencesAnyApplication")
  (preference-domain "com.apple.dt.Xcode"))
(allow pseudo-tty)

(allow file-read-metadata
  (literal "/")
  (literal "/var")
  (literal "/etc")
  (literal "/tmp")
  (literal "/private")
  (literal "/private/var")
  (literal "/private/var/db")
  (literal "/private/var/select")
  (literal "/Applications")
  (literal "/Library")
  (literal "/Library/Preferences")
  (literal "/Library/Developer")@ABOVE@)

;; dyld opens "/" as it starts.
(allow file-read-data (literal "/"))

(allow file-read*
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/System")
  (subpath "/Library/Developer/CommandLineTools")
  (subpath "/opt/homebrew")
  (subpath "/private/etc")
  (subpath "/private/var/db/dyld")
  (subpath "/private/var/db/timezone")
  (literal "/private/var/select/sh")
  (subpath "/dev")
  (literal "/private/var/db/xcode_select_link")
  (subpath "/Applications/Xcode.app")
  (literal "/Library/Preferences/com.apple.dt.Xcode.plist"))

;; /System/Volumes/Data is every other folder again under another name (/Users among
;; them), the disks hold every file, Homebrew's var (here or, on Intel Macs, under
;; /usr/local) holds its services' data, and /etc/ssh the machine's own keys. Homebrew's
;; etc stays readable like /etc: its programs fail on settings files they can't read.
(deny file-read* file-write*
  (subpath "/System/Volumes/Data")
  (regex #"^/dev/r?disk")
  (subpath "/opt/homebrew/var")
  (subpath "/usr/local/var")
  (regex #"^/private/etc/ssh/ssh_host_")
  (literal "/private/etc/master.passwd")
  (literal "/private/etc/sudoers")
  (subpath "/private/etc/sudoers.d"))

(allow file-map-executable
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/System")
  (subpath "/Library/Developer/CommandLineTools")
  (subpath "/Applications/Xcode.app")
  (subpath "/opt/homebrew"))

(allow file-write-data
  (literal "/dev/null")
  (literal "/dev/tty"))

@HIDDEN@
@READABLE@(allow file-read* file-write*
  (subpath @DIR@)
  (subpath @TMP@))

;; Programs built in the workspace load their own libraries and plugins.
(allow file-map-executable
  (subpath @DIR@)
  (subpath @TMP@))

;; Without these `ls -l` is several times slower: notifications and user names.
(allow ipc-posix-shm-read-data (ipc-posix-name "apple.shm.notification_center"))
(allow mach-lookup
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.system.opendirectoryd.libinfo"))

@NETWORK@
"#;

/// Network access on macOS: connections, plus the services that look up names and
/// check certificates. Not the keychain's (`com.apple.SecurityServer`), which would
/// hand stored passwords to helpers such as git's.
const NETWORK_ON: &str = r#"(allow network*)
(allow file-read* (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup
  (global-name "com.apple.dnssd.service")
  (global-name "com.apple.trustd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.networkd")
  (global-name "com.apple.nehelper"))"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn places<'a>(dir: &'a str, tmp: &'a str, source: Option<&'a str>) -> Places<'a> {
        Places { dir: Path::new(dir), tmp: Path::new(tmp), source: source.map(Path::new), readable: &[] }
    }

    #[test]
    fn the_macos_profile_names_the_mount_and_the_folders_above_it() {
        let profile = macos_profile(&places("/Users/ann/ws", "/private/var/folders/x/T/s", None), false).unwrap();
        assert!(profile.contains("(subpath \"/Users/ann/ws\")\n  (subpath \"/private/var/folders/x/T/s\")"));
        for above in ["/Users", "/Users/ann", "/private/var/folders", "/private/var/folders/x/T"] {
            assert!(profile.contains(&format!("(literal \"{above}\")")), "{above}");
        }
        assert!(!profile.contains("(literal \"/Users/ann/ws\")"));
        assert!(profile.contains("(deny network*)") && !profile.contains("(allow network*)"));
        assert_eq!(profile.matches("(deny file-read*").count(), 1, "only the data volume is hidden");
        for placeholder in ["@ABOVE@", "@HIDDEN@", "@READABLE@", "@DIR@", "@TMP@", "@NETWORK@"] {
            assert!(!profile.contains(placeholder), "{placeholder} is filled");
        }
        let open = macos_profile(&places("/w", "/t", None), true).unwrap();
        assert!(open.contains("(allow network*)") && !open.contains("(deny network*)"));
        assert!(!open.contains("SecurityServer"), "no keychain");
        assert!(profile.contains("(deny sysctl-read (sysctl-name-prefix \"kern.procargs\"))"));
        assert!(macos_profile(&places("/a\"b", "/t", None), false).is_err());
    }

    #[test]
    fn the_macos_profile_hides_the_source_before_allowing_the_mount() {
        let profile = macos_profile(&places("/data/ws", "/t", Some("/usr/local/share/books")), false).unwrap();
        let hidden = profile.find("(deny file-read* file-write* (subpath \"/usr/local/share/books\"))").unwrap();
        assert!(hidden < profile.find("(subpath \"/data/ws\")").unwrap(), "the mount's rule comes later and wins");
    }

    #[test]
    fn the_macos_profile_shows_readable_folders_inside_the_hidden_source() {
        let readable = [PathBuf::from("/Users/ann/.sandfs/repos/r/objects")];
        let mut places = places("/Users/ann/ws", "/t", Some("/Users/ann/.sandfs/repos/r"));
        places.readable = &readable;
        let profile = macos_profile(&places, false).unwrap();
        let hidden = profile.find("(deny file-read* file-write* (subpath \"/Users/ann/.sandfs/repos/r\"))").unwrap();
        let shown = profile.find("(allow file-read* (subpath \"/Users/ann/.sandfs/repos/r/objects\"))").unwrap();
        assert!(hidden < shown, "the later rule wins");
        assert!(profile.contains("(literal \"/Users/ann/.sandfs/repos/r\")"));
    }

    #[test]
    fn the_macos_profile_hides_the_data_volume_but_not_a_mount_on_it() {
        let profile = macos_profile(&places("/System/Volumes/Data/Users/ann/ws", "/t", None), false).unwrap();
        let system = profile.find("(subpath \"/System\")\n").unwrap();
        let data = profile.find("(subpath \"/System/Volumes/Data\")").unwrap();
        let mount = profile.find("(subpath \"/System/Volumes/Data/Users/ann/ws\")").unwrap();
        assert!(system < data && data < mount, "each later rule wins");
    }

    #[test]
    fn proxies_with_passwords_are_recognized() {
        for value in ["http://u:p@proxy:3128", "u@proxy:3128", "socks5://u@h/"] {
            assert!(holds_password(value.as_ref()), "{value}");
        }
        for value in ["http://proxy:3128", "proxy:3128", "http://proxy/a@b", "localhost,.example.com"] {
            assert!(!holds_password(value.as_ref()), "{value}");
        }
    }

    #[test]
    fn placeholders_in_paths_stay_as_they_are() {
        let profile = macos_profile(&places("/Users/a/@TMP@/ws", "/t", None), false).unwrap();
        assert!(profile.contains("(subpath \"/Users/a/@TMP@/ws\")"));
        assert_eq!(fill("@A@-@B@ x@y", &[("@A@", "@B@"), ("@B@", "b")]), "@B@-b x@y");
    }
}
