//! How sandfs runs its program again: for the background process that serves mounts,
//! for git's file monitor in a git mount, and on macOS for `shell`. A program that
//! embeds sandfs is run again in its place, with the words it gave
//! [`handle_relaunch`] first.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result};

use crate::store::Home;

/// The words that come before sandfs's own when it runs its program again.
static PREFIX: OnceLock<Vec<OsString>> = OnceLock::new();

/// Call this first thing in `main` of a program that uses sandfs, before anything
/// else reads the arguments.
///
/// sandfs serves mounts from a background process that outlives whoever asked for the
/// mount, and starts it by running this program again with `prefix` followed by words
/// of its own; git's file monitor in a git mount runs it the same way. When this
/// process is one of those runs, this does what it was run for and exits. Otherwise
/// it remembers `prefix` and returns.
///
/// The `sandfs` command gives no prefix. A program with commands of its own gives one
/// that none of them uses, such as `&["sandfs"]`, so that `PROGRAM sandfs daemon` is
/// sandfs's and everything else is its own.
pub fn handle_relaunch(prefix: &[&str]) {
    let _ = PREFIX.set(prefix.iter().map(OsString::from).collect());
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(rest) = args.strip_prefix(self::prefix()) else { return };
    let words: Vec<&str> = rest.iter().map(|word| word.to_str().unwrap_or("")).collect();
    let result = match words.as_slice() {
        ["daemon"] => Home::locate().and_then(|home| crate::ops::serve_mounts(home, false)),
        ["daemon", "--serve"] => Home::locate().and_then(|home| crate::ops::serve_mounts(home, true)),
        ["shell-exec", "--", _, ..] => crate::shell::exec_in_new_session(&rest[2..]),
        // Git adds the token of its last answer, which the answer doesn't depend on.
        ["git-fsmonitor", version, ..] => Home::locate().and_then(|home| crate::ops::git_fsmonitor(&home, version)),
        _ => return,
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("sandfs: {err:#}");
            std::process::exit(1)
        }
    }
}

/// See [`handle_relaunch`].
pub(crate) fn prefix() -> &'static [OsString] {
    PREFIX.get().map_or(&[], Vec::as_slice)
}

/// This program, as a path. Linux names a binary replaced since it started, as by
/// `cargo install`, with a ` (deleted)` suffix; its replacement at the same path is
/// the one to run.
pub(crate) fn program() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding this program")?;
    match exe.to_str().and_then(|exe| exe.strip_suffix(" (deleted)")) {
        Some(exe) => Ok(PathBuf::from(exe)),
        None => Ok(exe),
    }
}

/// A command that runs this program again for sandfs, its words to follow.
pub(crate) fn command() -> Result<Command> {
    let mut command = Command::new(std::env::current_exe().context("finding this program")?);
    command.args(prefix());
    Ok(command)
}
