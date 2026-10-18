//! `sandfs-mcp`: serve a sandfs workspace to an MCP client over stdin and stdout.

use std::process::ExitCode;
use std::sync::Arc;

use sandfs_core::{DirSource, Workspace};
use sandfs_mcp::Server;

const USAGE: &str = "\
usage: sandfs-mcp [--dir MOUNT=PATH]...

Serve a copy-on-write workspace to an MCP client over stdio. Each --dir mounts a
host folder read-only and reads it lazily; the agent's changes live in memory for
the session. To serve other data, embed the sandfs-mcp library with your own sources.

  --dir /data=./datasets   mount the host folder ./datasets at /data
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
        if flag != "--dir" {
            return Err(format!("unknown argument {flag:?}"));
        }
        let spec = args.next().ok_or("--dir needs MOUNT=PATH")?;
        let (mount, dir) = spec.split_once('=').ok_or_else(|| format!("--dir {spec}: expected MOUNT=PATH"))?;
        let source = DirSource::new(dir).map_err(|e| e.to_string())?;
        workspace.mount(mount, Arc::new(source)).map_err(|e| e.to_string())?;
    }
    Ok(workspace)
}
