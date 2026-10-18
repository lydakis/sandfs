//! `sandfs-mcp`: serve a sandfs workspace to an MCP client over stdin and stdout.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use sandfs_core::{DirSource, Source, Workspace};
use sandfs_git::GitSource;
use sandfs_mcp::Server;

const USAGE: &str = "\
usage: sandfs-mcp [--dir MOUNT=PATH]... [--git MOUNT=REPO[@REV]]...

Serve a copy-on-write workspace to an MCP client over stdio. Mounted sources are
read-only and read lazily; the agent's changes live in memory for the session.

  --dir /work=./project    mount a host directory
  --git /repo=./clone@main mount a commit of a local clone (default HEAD)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let workspace = match configure(&args) {
        Ok(workspace) => workspace,
        Err(message) => {
            eprintln!("sandfs-mcp: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match Server::new(workspace).serve(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sandfs-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

fn configure(args: &[String]) -> Result<Workspace, String> {
    let mut workspace = Workspace::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag != "--dir" && flag != "--git" {
            return Err(format!("unknown argument {flag:?}"));
        }
        let spec = args.next().ok_or_else(|| format!("{flag} needs MOUNT=SOURCE"))?;
        let (mount, location) = spec.split_once('=').ok_or_else(|| format!("{flag} {spec}: expected MOUNT=SOURCE"))?;
        let source: Arc<dyn Source> = if flag == "--dir" {
            Arc::new(DirSource::new(location).map_err(|e| e.to_string())?)
        } else {
            let (repo, rev) = split_rev(location);
            Arc::new(GitSource::open(repo, rev).map_err(|e| e.to_string())?)
        };
        workspace.mount(mount, source).map_err(|e| e.to_string())?;
    }
    Ok(workspace)
}

/// Split `REPO@REV`, unless the whole string names an existing path.
fn split_rev(location: &str) -> (&str, &str) {
    if Path::new(location).exists() {
        return (location, "HEAD");
    }
    match location.rsplit_once('@') {
        Some((repo, rev)) if !repo.is_empty() && !rev.is_empty() => (repo, rev),
        _ => (location, "HEAD"),
    }
}
