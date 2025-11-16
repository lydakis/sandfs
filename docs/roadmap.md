# sandfs Roadmap

This document captures the practical roadmap for `sandfs` so we can measure progress toward a "VFS-first" agent environment where the virtual tree feels like the whole world.

## Where we are today

- **Core VFS** – in-memory directories, provider-backed nodes, optimistic versions, snapshots, host export/import helpers.
- **Policy layer** – read/write/append policies plus visibility views and path hooks so hosts can mirror activity elsewhere.
- **Command surface** – built-in shell commands: `pwd`, `cd`, `ls`, `tree`, `cat`, `touch`, `mkdir`, `rm`, `write`, `append`, `grep`, `rg`, `python`, `python3`, `host`, `bash`, `sh`, and `help`.
- **Host bridge** – `host` (and fallback for unknown commands) materializes the tree to a temp dir and runs native binaries; powerful but unsafe for untrusted users.

## Principles for upcoming work

1. **Pure VFS illusion by default** – agents should accomplish >80% of tasks without touching host fallback.
2. **Small, opinionated command set** – implement the utilities agents actually need (read/search/light edits) instead of replicating full POSIX semantics.
3. **Explicit escape hatches** – keep host fallback as an opt-in convenience and clearly document its risks.
4. **Pluggable execution backends** – over time, allow swapping in WASI/containers/namespaces without changing the shell API.

## Near-term backlog (v0.x)

Focus: expand the pure-Python toolchain so normal agent workflows never require host commands.

- **Navigation & metadata**
  - Add `stat`/`info` for per-path metadata (size, policy, version, timestamps).
  - Support `ls -a`/`ls -R` flags plus a `du`-lite summary command.
- **Viewing & paging**
  - Implement `head`, `tail`, and a pager-style `view` for long files.
  - Extend `cat` with line numbers and highlighting options useful to LLMs.
- **Search & filtering**
  - Ship a `find`/`fd`-style command for filtered traversals (name/glob/extension filters).
  - Enhance `rg` with include/exclude globs, max-results, and JSON output for agents.
- **Editing & diffs**
  - Introduce `cp`, `mv`, `apply_patch`, and `diff` so agents can restructure files safely.
  - Provide `sed`-lite replacements and multi-file write helpers.
- **File creation ergonomics**
  - Extend `write`/`append` with heredoc (`<<EOF`) support, timestamp helpers, and batch command execution so agents can create structured notes without `/bin/sh`.
  - Add native `echo`/`printf` utilities so scripted workflows stay inside the VFS.
- **Snapshots & history**
  - Surface shell commands for `snapshot`, `restore`, and `diff-snap <id>` (wrapping the existing VFS APIs).
- **Ergonomics**
  - Structured command responses (rich metadata that the CLI/UI can pretty-print) and better error messaging.
  - Diagnostics helpers like `status`/`info` to expose visible roots, policies, and current mounts without falling back to host shells.

## Mid-term explorations

- **Structured data helpers** – `select`/`jq`-style view of JSON/YAML/TOML, CSV previewers, formatting utilities.
- **Archive / packaging** – zip/tar helpers entirely within the VFS so agents can bundle outputs without host tooling.
- **Execution backends** – research a WASI-based backend (reuse BusyBox/coreutils) and, for Linux deployments, a namespace/container-backed host runner for "hard" isolation.

## Long-term ideas

- **Policy-aware agents** – surface node policies directly in command responses so planners can avoid forbidden paths proactively.
- **Observability** – pluggable telemetry for command usage, timing, and write hooks that integrate with host logs.
- **Multi-tenant sandboxes** – carve multiple views over the same VFS (per agent/principal) with isolated shells.

This roadmap will evolve as we learn what commands agents rely on most. Contributions or feedback can be filed as issues referencing the relevant section above.
