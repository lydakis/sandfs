//! The one background process that serves every mount of a home.
//!
//! `mount` and `fork` ask it to mount a workspace over a Unix socket that only its
//! owner can reach, and start it first when none is running. Each mount runs on
//! threads of its own, and workspaces over the same commit share it (see [`Bases`]).
//! The process exits once its last mount is gone, so nothing stays running.

use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::store::{Bases, Home};

/// How long the process stays after its last mount, so a mount right after finds it.
const LINGER: Duration = Duration::from_secs(2);
/// How long a new process may take to start answering.
const START: Duration = Duration::from_secs(10);
/// How long a mount may take, which includes fetching a remote commit's folders.
const MOUNT: Duration = Duration::from_secs(120);

/// A request to mount the workspace `name` at `dir`, which exists and is canonical.
/// The directory goes as its bytes, since a path needn't be UTF-8.
#[derive(Serialize, Deserialize)]
struct Request {
    name: String,
    dir: Vec<u8>,
}

impl Request {
    fn dir(&self) -> PathBuf {
        PathBuf::from(std::ffi::OsStr::from_bytes(&self.dir))
    }
}

/// The answer, once the mount is up or has failed.
#[derive(Serialize, Deserialize)]
struct Reply {
    error: Option<String>,
}

/// Where the background process writes what it reports.
pub fn log_path(home: &Home) -> PathBuf {
    home.run_dir().join("server.log")
}

fn socket_path(home: &Home) -> PathBuf {
    home.run_dir().join("server.sock")
}

/// The directory holding the socket, private to its owner, so nobody else can ask for
/// mounts.
fn run_dir(home: &Home) -> Result<PathBuf> {
    let dir = home.run_dir();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Mount `name` at `dir` in the home's background process, starting the process if
/// none is running, and wait until the mount is up.
pub fn mount(home: &Home, name: &str, dir: &Path) -> Result<()> {
    let request = serde_json::to_string(&Request { name: name.to_owned(), dir: dir.as_os_str().as_bytes().to_vec() })?;
    let started = Instant::now();
    let mut server: Option<Child> = None;
    // A running process answers at once, and a new one within a few milliseconds.
    let mut wait = Duration::from_millis(1);
    loop {
        // A process on its way out closes connections without answering, and then
        // another is started.
        if let Ok(stream) = UnixStream::connect(socket_path(home))
            && let Some(reply) = ask(stream, &request)?
        {
            return reply.error.map_or(Ok(()), |err| Err(anyhow!(err)));
        }
        // Start a process, unless the one started before may still be coming up.
        if !server.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None))) {
            server = Some(start(home)?);
        }
        if started.elapsed() > START {
            bail!("the mount process did not start; see {}", log_path(home).display());
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    }
}

/// Send `request` and read the reply, or `None` if the connection closes first.
fn ask(mut stream: UnixStream, request: &str) -> Result<Option<Reply>> {
    stream.set_read_timeout(Some(MOUNT))?;
    if stream.write_all(format!("{request}\n").as_bytes()).is_err() {
        return Ok(None);
    }
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(serde_json::from_str(&line).context("reading the mount process's answer")?)),
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            bail!("timed out waiting for the mount")
        }
        Err(err) if err.kind() == ErrorKind::ConnectionReset => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Start the background process, detached from this one's terminal.
fn start(home: &Home) -> Result<Child> {
    run_dir(home)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path(home))?;
    let mut command = Command::new(std::env::current_exe()?);
    // glibc's allocator raises the size it maps directly once a large block is freed,
    // so after the first mount ends, each new mount's 16 MiB request buffer comes
    // zeroed from memory the process keeps, and a process outliving its mounts grew by
    // hundreds of MiB as they came and went. A fixed threshold, above any one read or
    // write, keeps those buffers mapped (and untouched, so free) and lets freed memory
    // go back.
    #[cfg(target_os = "linux")]
    if std::env::var_os("MALLOC_MMAP_THRESHOLD_").is_none() {
        command.env("MALLOC_MMAP_THRESHOLD_", "4194304");
    }
    command
        .arg("daemon")
        // Started from inside a mount, it would otherwise keep that mount busy.
        .current_dir("/")
        .env("SANDFS_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0) // keep running when the terminal goes away
        .spawn()
        .context("starting the mount process")
}

/// Serve mounts until none is left: the background process itself.
pub fn run(home: Home) -> Result<()> {
    let dir = run_dir(&home)?;
    let lock = File::options().write(true).create(true).truncate(false).open(dir.join("server.lock"))?;
    // One process serves the home. One that is on its way out holds the lock for a
    // moment longer; one still running keeps it, and this one leaves.
    let deadline = Instant::now() + Duration::from_secs(1);
    while lock.try_lock().is_err() {
        if Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Whatever socket is left belongs to a process that has exited.
    let socket = socket_path(&home);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).context("listening for mounts")?;
    let state = Arc::new(State::default());
    let bases = Arc::new(Bases::default());
    let accepting = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            // Closing: the connection drops unanswered, and its client starts a new process.
            let Some(ticket) = accepting.enter() else { break };
            let (home, bases) = (home.clone(), bases.clone());
            std::thread::spawn(move || answer(&stream, &home, &bases, ticket));
        }
    });
    state.wait_until_idle(LINGER);
    // Removed while this process still holds the lock, so it is never a successor's.
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Read one request from `stream`, start the mount and answer once it is up or failed.
fn answer(mut stream: &UnixStream, home: &Home, bases: &Arc<Bases>, ticket: Ticket) {
    let request = read_request(stream);
    let dir = request.as_ref().ok().map(Request::dir);
    let mounted = request.and_then(|request| start_mount(home, bases, request, ticket));
    let up = mounted.is_ok();
    let reply = Reply { error: mounted.err().map(|err| format!("{err:#}")) };
    let told =
        serde_json::to_string(&reply).is_ok_and(|reply| stream.write_all(format!("{reply}\n").as_bytes()).is_ok());
    // A requester that gave up (timed out, or was interrupted) has reported a failure,
    // so a mount that came up after all is taken down again.
    if let Some(dir) = dir.filter(|_| up && !told) {
        eprintln!("sandfs: nobody waited for the mount at {}; unmounting it", dir.display());
        let _ = crate::host::unmount(&dir, false);
    }
}

fn read_request(stream: &UnixStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).context("reading a mount request")
}

/// Serve the requested mount on a thread of its own, which keeps `ticket` for as long
/// as it serves, and wait until the mount is up or has failed.
fn start_mount(home: &Home, bases: &Arc<Bases>, request: Request, ticket: Ticket) -> Result<()> {
    let (done, outcome) = mpsc::channel();
    let up = done.clone();
    let (home, bases) = (home.clone(), bases.clone());
    std::thread::Builder::new()
        .name(format!("mount {}", request.name))
        .spawn(move || {
            let _ticket = ticket;
            let served = crate::serve(&home, &request.name, &request.dir(), &bases, move || {
                let _ = up.send(Ok(()));
            });
            if let Err(err) = &served {
                eprintln!("sandfs: {}: {err:#}", request.name);
            }
            // A mount that never came up reports why; for one that did, this goes unread.
            let _ = done.send(served);
        })
        .context("starting a mount")?;
    outcome.recv().unwrap_or_else(|_| Err(anyhow!("the mount stopped unexpectedly")))
}

/// How many mounts, and requests for one, the process is serving, and whether it has
/// stopped taking more.
#[derive(Default)]
struct State {
    busy: Mutex<(usize, bool)>,
    changed: Condvar,
}

/// One mount or request, counted until dropped.
struct Ticket(Arc<State>);

impl State {
    /// Count a new request, unless the process is closing.
    fn enter(self: &Arc<Self>) -> Option<Ticket> {
        let mut busy = self.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if busy.1 {
            return None;
        }
        busy.0 += 1;
        Some(Ticket(self.clone()))
    }

    /// Wait until nothing has been served for `linger`, then stop taking requests.
    fn wait_until_idle(&self, linger: Duration) {
        let mut busy = self.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if busy.0 > 0 {
                busy = self.changed.wait_while(busy, |busy| busy.0 > 0).unwrap_or_else(|err| err.into_inner());
                continue;
            }
            let (guard, idle) = self
                .changed
                .wait_timeout_while(busy, linger, |busy| busy.0 == 0)
                .unwrap_or_else(|err| err.into_inner());
            busy = guard;
            if idle.timed_out() {
                busy.1 = true;
                return;
            }
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut busy = self.0.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        busy.0 -= 1;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_carry_any_directory_name() {
        let dir = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"));
        let request = Request { name: "ws".into(), dir: dir.as_os_str().as_bytes().to_vec() };
        let sent: Request = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(sent.dir(), dir);
    }

    #[test]
    fn the_process_closes_only_after_lingering_idle() {
        let state = Arc::new(State::default());
        let ticket = state.enter().unwrap();
        let waiter = {
            let state = state.clone();
            std::thread::spawn(move || {
                state.wait_until_idle(Duration::from_millis(50));
                Instant::now()
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "a mount keeps the process");
        // Timed from the drop, not from when the thread started, which a busy machine
        // can delay.
        let idle = Instant::now();
        drop(ticket);
        assert!(waiter.join().unwrap() >= idle + Duration::from_millis(50), "it lingers once idle");
        assert!(state.enter().is_none(), "a closed process takes no more requests");
    }
}
