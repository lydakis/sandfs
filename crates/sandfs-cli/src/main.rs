//! `sandfs`: mount copy-on-write workspaces over read-only sources, such as a host
//! folder.
//!
//! The commands are the core's primitives. `mount` creates a workspace over a source
//! and serves it as a directory (FUSE on Linux, NFS on macOS), `fork` mounts a copy of one without copying its source, and `changes` lists the
//! files a workspace changed. A working copy on disk is just a fork mounted at a path.
//! `serve --mcp` hands a workspace to an agent as MCP file tools instead of a mount.
//! One background process serves every mount. Each command is a method of
//! [`sandfs::Home`]; this is the layer that reads arguments and prints.

use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sandfs::{ChangeKind, ChangesOptions, CleanOutcome, ForkOptions, Home, MountOptions, ServeOptions};

#[derive(Parser)]
#[command(name = "sandfs", version, about = "Lazy, copy-on-write workspaces mounted as directories")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a workspace over a source and mount it at DIR, or remount a workspace.
    ///
    /// SOURCE is a host folder, which is read lazily and never written to, or the
    /// name of an existing workspace (write ./NAME for a folder that shares a
    /// workspace's name). git:<path or url>[@rev] mounts a commit, and so does a URL
    /// on its own, such as https://github.com/owner/repo; remote repositories become
    /// partial clones, so file contents are fetched only when a directory or file is
    /// first read.
    Mount {
        source: String,
        dir: PathBuf,
        /// Name for the new workspace (default: DIR's last component).
        #[arg(long)]
        name: Option<String>,
        /// A file or folder of the source, relative to it, that programs may read but
        /// not change, even in the workspace: changes there fail as on a read-only file
        /// system. Repeat for several; forks keep them.
        #[arg(long, value_name = "PATH")]
        read_only: Vec<String>,
        /// Serve in the foreground instead of in the background.
        #[arg(long, short)]
        foreground: bool,
    },
    /// Fork a workspace, with its changes so far, and mount the fork at DIR. The source
    /// is never copied; each saved change is linked into the fork.
    ///
    /// FROM can also be a git repository's folder that sandfs doesn't serve, such as a
    /// clone: the fork is then its HEAD commit plus the files the folder has changed
    /// since (changed, new or deleted; ignored files stay out). Either way the fork is a
    /// copy of FROM as it is now, and later changes to FROM don't reach it.
    ///
    /// Without DIR the fork is only kept, not mounted: a snapshot of the workspace as it
    /// is now, which `mount NAME DIR` or `fork NAME DIR` brings back later.
    Fork {
        /// The workspace to fork (its name, or the directory it is mounted on or a folder
        /// in it), or a git repository's folder.
        from: String,
        dir: Option<PathBuf>,
        /// Name for the fork (default: DIR's last component; required without DIR).
        #[arg(long)]
        name: Option<String>,
        /// A file or folder, relative to the workspace, that programs may read but not
        /// change, as for `mount`; added to those FROM already has. Repeat for several.
        #[arg(long, value_name = "PATH")]
        read_only: Vec<String>,
        /// With a git source: start git in the fork on a new branch NAME at the commit
        /// FROM is on, as `git switch -c NAME` would, keeping its files and staged
        /// changes, so the fork's commits can be pushed with `git push origin NAME`.
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        /// Serve in the foreground instead of in the background.
        #[arg(long, short)]
        foreground: bool,
    },
    /// List the files a workspace added (A), modified (M) or deleted (D) relative to its source.
    Changes {
        /// A workspace name or the directory it is mounted on.
        workspace: String,
        /// Only these files, or the files in these folders, relative to the workspace.
        paths: Vec<String>,
        /// Compare against this workspace instead of the source: a fork kept as a
        /// checkpoint, or another attempt forked from the same one.
        #[arg(long, value_name = "WORKSPACE")]
        since: Option<String>,
        /// Also write the changes listed into DIR: added and modified files are copied
        /// there and deleted ones removed, so a copy of the source in DIR (or of the
        /// --since workspace) comes to match the workspace. Nothing else is written,
        /// and never through a link in DIR. A file in DIR that differs both from what
        /// the workspace started from and from what it would become, as one edited
        /// there since, is never replaced or removed: the copy is refused, naming it.
        #[arg(long, value_name = "DIR")]
        copy_to: Option<PathBuf>,
        /// With --copy-to, replace or remove such files too.
        #[arg(long, requires = "copy_to")]
        force: bool,
    },
    /// Move a workspace onto a newer version of its source, keeping its own changes.
    ///
    /// A mounted workspace moves in place: its changes are saved, and from then on the
    /// mount reads as the newer source, while programs in it (a shell in one of its
    /// folders, files left open) carry on.
    ///
    /// Without SOURCE, the source is looked up again as it was given, so a git: source
    /// or URL moves to the newest commit of its branch (a fixed @revision stays where it
    /// is), and a fork of a plain folder takes a new copy of the folder as it is now.
    /// SOURCE names another folder, or another commit of the same repository.
    /// Files that changed there and not in the workspace now read as they are there.
    /// The workspace's own changes stay, and one that matches the newer source stops
    /// counting as a change. Paths changed in both are printed as `C path`; the
    /// workspace's version of those stays. Forks of the workspace aren't moved.
    Update {
        /// A workspace name or the directory it was mounted on.
        workspace: String,
        source: Option<String>,
    },
    /// List workspaces and where they are mounted.
    List,
    /// Unmount a workspace. Its changes stay saved; `mount NAME DIR` brings it back.
    Unmount {
        /// A workspace name or the directory it is mounted on.
        workspace: String,
    },
    /// Delete an unmounted workspace and its changes.
    Rm {
        /// The workspace's name, or with --unmount the directory it is mounted on.
        name: String,
        /// Unmount the workspace first if it is mounted.
        #[arg(long)]
        unmount: bool,
    },
    /// Free the disk space remote repositories' downloaded files take up.
    ///
    /// For each remote repository none of whose workspaces is in use, drop the file
    /// contents fetched so far and keep only the commits and folder listings its
    /// workspaces need, so they still mount at once and fetch a file again when it is
    /// next read. A repository no workspace uses any more is deleted. Workspaces and
    /// their changes, commits among them, are never touched.
    Clean,
    /// Serve an unmounted workspace to an agent as MCP file tools over stdin and stdout,
    /// or create one over a source and serve that.
    ///
    /// The agent gets read, list, glob, grep, write, edit, move, delete and changes,
    /// with the source's files under /mnt. Every change is saved before its reply, so
    /// `changes`, `fork` and `mount` see it afterwards.
    Serve {
        /// Speak MCP (the only protocol so far).
        #[arg(long)]
        mcp: bool,
        /// A workspace name, or a source as for `mount`.
        workspace: String,
        /// Name for a new workspace (default: the source's last component).
        #[arg(long)]
        name: Option<String>,
        /// For a new workspace, a path the agent may read but not change, as for `mount`.
        #[arg(long, value_name = "PATH")]
        read_only: Vec<String>,
    },
    /// Run a shell, or a command, that sees a mounted workspace and none of the
    /// machine's other files.
    ///
    /// The program starts in the mount and may change anything there that isn't
    /// read-only. It can run the system's programs but not read your home folder, other
    /// folders, or the workspace's source, and it gets no network unless --network is
    /// given. HOME is an empty folder outside the mount, deleted afterwards along with
    /// anything else written outside it. Only a few environment variables pass through
    /// (PATH, TERM, the locale, USER and SHELL, and with --network the proxy settings),
    /// so tokens and keys in yours stay out.
    ///
    /// The operating system does the confining: bubblewrap on Linux (install the
    /// bubblewrap package), the built-in sandbox on macOS.
    Shell {
        /// A mounted workspace's name or folder.
        workspace: String,
        /// Let the program use the network.
        #[arg(long)]
        network: bool,
        /// The command to run, after `--`, instead of your shell.
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<std::ffi::OsString>,
    },
}

fn main() {
    // The mount process, git's file monitor and `shell` on macOS run sandfs again.
    sandfs::handle_relaunch(&[]);
    if let Err(err) = run(Cli::parse()) {
        eprintln!("sandfs: {err:#}");
        std::process::exit(1);
    }
}

/// Print a listing. A reader that stops early, like `sandfs changes NAME | head`, isn't
/// an error.
fn print_out(text: &str) -> Result<()> {
    match std::io::stdout().lock().write_all(text.as_bytes()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = Home::locate()?;
    match cli.command {
        Cmd::Mount { source, dir, name, read_only, foreground } => {
            let mounted = home.mount(&source, &dir, &MountOptions { name, read_only, foreground })?;
            if !foreground {
                println!("{} mounted at {}", mounted.name, mounted.dir.display());
            }
            Ok(())
        }
        Cmd::Fork { from, dir, name, read_only, branch, foreground } => {
            let forked = home.fork(&from, dir.as_deref(), &ForkOptions { name, read_only, branch, foreground })?;
            match forked.dir {
                Some(_) if foreground => {}
                Some(dir) => println!("{} mounted at {}", forked.name, dir.display()),
                None => println!("{} forked from {}", forked.name, forked.from),
            }
            Ok(())
        }
        Cmd::Changes { workspace, paths, since, copy_to, force } => {
            let found = home.changes(&workspace, &ChangesOptions { paths, since, copy_to, force })?;
            let mut out = String::new();
            for change in &found.changes {
                let marker = match change.kind {
                    ChangeKind::Added => 'A',
                    ChangeKind::Modified => 'M',
                    ChangeKind::Deleted => 'D',
                };
                writeln!(out, "{marker} {}", change.path)?;
            }
            print_out(&out)?;
            if found.held_back > 0 {
                eprintln!(
                    "sandfs: left out {} changes in .git, whose settings name programs git runs; copy them by hand if you mean to",
                    found.held_back
                );
            }
            for rel in &found.git_settings {
                eprintln!("sandfs: {rel} changed; git runs the programs it names, so read it before running git here");
            }
            Ok(())
        }
        Cmd::Update { workspace, source } => {
            let name = home.find(&workspace)?;
            let updated = home.update(&name, source.as_deref())?;
            let mut out = String::new();
            for rel in &updated.both {
                writeln!(out, "C {rel}")?;
            }
            print_out(&out)?;
            let upstream = updated.upstream.map(|n| format!(", {n} files changed there")).unwrap_or_default();
            eprintln!(
                "sandfs: moved {name} from {} to {}{upstream}; {} of its changes matched and are changes no longer, {} changed on both sides",
                updated.from,
                updated.to,
                updated.matched,
                updated.both.len()
            );
            Ok(())
        }
        Cmd::List => {
            let mut out = String::new();
            for workspace in home.workspaces()? {
                let mounted = workspace.mounted.map(|dir| dir.display().to_string());
                writeln!(out, "{}\t{}\t{}", workspace.name, workspace.source, mounted.as_deref().unwrap_or("-"))?;
            }
            print_out(&out)
        }
        Cmd::Unmount { workspace } => home.unmount(&workspace),
        Cmd::Rm { name, unmount } => home.remove(&name, unmount),
        Cmd::Clean => {
            let mut out = String::new();
            for cleaned in home.clean()? {
                let shown = &cleaned.repository;
                match cleaned.outcome {
                    CleanOutcome::Deleted { freed } => {
                        writeln!(out, "{shown}: deleted, {} freed (no workspace uses it)", size(freed))?
                    }
                    CleanOutcome::Kept { busy } => {
                        writeln!(out, "{shown}: kept, {busy} is in use (unmount it to clean)")?
                    }
                    CleanOutcome::Shrunk { freed } => writeln!(out, "{shown}: {} freed", size(freed))?,
                }
            }
            print_out(&out)
        }
        Cmd::Serve { mcp, workspace, name, read_only } => {
            if !mcp {
                anyhow::bail!("say what to serve: sandfs serve --mcp {workspace}");
            }
            home.serve_mcp(&workspace, &ServeOptions { name, read_only })
        }
        Cmd::Shell { workspace, network, command } => std::process::exit(home.shell(&workspace, network, &command)?),
    }
}

/// `bytes` for people: 1.5 MiB.
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0),
    }
}
