# sandfs Vision

`sandfs` aims to provide an extensible, self-contained virtual filesystem (VFS) that can be embedded in agentic workflows. The VFS is designed to be backed by in-memory state, dynamically generated nodes, or live data sources that are exposed via provider callables. Agent developers can expose a single `exec` tool that proxies commands into the sandbox, enabling AIs to manipulate a private filesystem with familiar UNIX idioms.

## Core Principles

1. **Virtual-first** – The filesystem lives in memory and never touches the host disk unless explicitly exported.
2. **Dynamic data** – Nodes can be backed by callables (or adapters around DB queries, APIs, etc.) that materialize file contents or directory listings on demand.
3. **Command friendly** – The sandbox ships with a tiny command runner that understands a subset of GNU-style commands (`cd`, `ls`, `cat`, `grep`, `rg`, `tree`, etc.). New commands can be registered.
4. **Execution context isolation** – Python or shell snippets can be executed against the VFS without escaping it. All side-effects remain local to the sandbox state object.
5. **Composable layers** – Multiple providers can be mounted at different paths, enabling hybrid topologies (e.g., static project template + live DB dumps + scratch space).

## Proposed Architecture

- `sandfs.nodes` keeps lightweight dataclasses for directories, files, and symbolic links. Nodes expose a uniform `read()` / `write()` / `iterdir()` API and support lazy population.
- `sandfs.providers` defines the provider protocol (callables receiving a `NodeContext` and returning bytes/str) plus adapters for common scenarios (e.g., `StaticText`, `CallableProvider`).
- `sandfs.vfs.VirtualFileSystem` manages the tree, the current working directory, path resolution, mounting/unmounting providers, and basic file operations.
- `sandfs.shell.SandboxShell` hosts a registry of command handlers, parses user commands via `shlex`, and executes them against the VFS, returning a `CommandResult` object.
- `sandfs.pyexec.PythonExecutor` offers a helper to run Python code with a curated globals dict that includes the VFS helpers but forbids direct disk I/O by default.

## Roadmap Snapshot

1. Build the foundational VFS and directory/file nodes with provider support.
2. Layer on top a shell/executor that mimics common filesystem commands.
3. Add serialization/import/export helpers so sandboxes can be persisted or hydrated from templates.
4. Expand command coverage (pipelines, redirection, e.g., `head`, `tail`, `sed`) and integrate with long-lived agent sessions.
5. Offer host integration points, such as bridging selected directories to the real FS through adapters guarded by policies.

This document captures the initial architectural intent and can be revised as the package evolves.
