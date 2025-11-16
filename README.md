# sandfs

`sandfs` is an experimental Python package that implements an entirely virtual filesystem (VFS) that can be embedded inside AI agent tooling. Each sandbox keeps its own private directory tree, supports dynamic nodes that are hydrated on demand, and exposes a small UNIX-like command surface (`cd`, `ls`, `cat`, `grep`, `rg`, etc.) so agents can reuse familiar workflows.

> **Status**: design prototype. APIs are subject to change.

## Why?

Agent builders frequently need a scratch filesystem for planning, iterating on generated code, and testing hypotheses. Shipping a simple `exec` tool that proxies every bash command to the host disk is unsafe. `sandfs` keeps those operations inside a controlled, in-memory sandbox while still feeling like a mini Linux environment.

## Features

- In-memory directories and files that never touch the host disk unless exported.
- Dynamic nodes backed by callables (e.g., query a DB, fetch from an API, or generate text on the fly).
- Pure-Python shell/executor that understands a handful of GNU-style commands and can be extended with your own.
- Optional Python execution helper that evaluates snippets against the sandbox state only.
- Bridge to real GNU utilities via `host -p <path> <command>` which materializes the subtree and runs the host binary against it.
- Node policies (read-only, append-only, visibility labels) plus shell views so different agents see only the nodes they are allowed to.
- Write hooks and optimistic versions so hosts can flush files to external stores with conflict detection.
- Serialization helpers to snapshot or hydrate sandboxes (planned).

## Quickstart

```python
from sandfs import VirtualFileSystem, SandboxShell

vfs = VirtualFileSystem()
vfs.write_file("/notes/todo.txt", "- build VFS\n- add rg support\n")

shell = SandboxShell(vfs)
print(shell.exec("ls /notes").stdout)
print(shell.exec("rg 'VFS' /notes").stdout)
```

### Agent shell usage

```python
from sandfs import ProvidedNode, SandboxShell, VirtualFileSystem

vfs = VirtualFileSystem()
vfs.write_file("/workspace/app.py", "print('hello')\n")

def logs_provider(ctx):
    return {"latest.log": ProvidedNode.file(content="generated on demand")}

vfs.mount_directory("/workspace/logs", logs_provider)

shell = SandboxShell(vfs)
shell.exec("cd /workspace")
print(shell.exec("ls").stdout)
print(shell.exec("tree").stdout)
```

### Running host GNU tools

`SandboxShell` exposes `host` to materialize a subtree onto the host disk and invoke any installed command:

```
host -p /workspace grep -n TODO app.py
```

The example above exports `/workspace` into a temporary directory, runs the system `grep` inside it, then discards the files.

### Policies & views

```python
from sandfs import NodePolicy, SandboxShell, VirtualFileSystem, VisibilityView

vfs = VirtualFileSystem()
vfs.write_file("/blue/identity/persona.md", "persona")
vfs.set_policy("/blue/identity/persona.md", NodePolicy(writable=False, visibility="private"))

shell = SandboxShell(vfs, view=VisibilityView({"public"}))
print(shell.exec("ls /blue").stdout)  # persona.md hidden
```

## Repository layout

```
.
├── docs/               # design/vision notes
├── sandfs/             # library sources
└── tests/              # runtime smoke tests
```

## Local development

```
python -m venv .venv
source .venv/bin/activate
uv pip install -e .[dev]
uv run pytest
```

## License

MIT
### Persistence hooks

Register a write hook to flush files into your own store and use optimistic versions to avoid clobbering concurrent updates:

```python
from sandfs import VirtualFileSystem
from sandfs.hooks import WriteEvent

vfs = VirtualFileSystem()

def flush(event: WriteEvent) -> None:
    save_to_db(event.path, event.content, event.version)

vfs.register_write_hook("/blue/work", flush)
vfs.write_file("/blue/work/note.md", "draft")
vfs.write_file("/blue/work/note.md", "final", expected_version=1)
```
