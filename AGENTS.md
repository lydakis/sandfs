# Repository Guidelines

## Project Structure & Module Organization
- `crates/` holds the implementation. `sandfs-core` has the three primitives (sources, workspaces with a copy-on-write overlay, and the changes `changes()` reports) plus the directory and in-memory sources, `sandfs-git` is an optional lazy git source, `sandfs-mcp` is the MCP server with the text tools (glob, grep, edit), `sandfs` is the library of everything the command does (workspaces saved on disk, the git and GitHub sources, mounting over FUSE or NFS with its background process, fork, update and changes), `sandfs-cli` is the `sandfs` command, a thin layer over it, `sandfs-py` is the PyO3 extension, including sources written in Python, and `sandfs-bench` (unpublished) benchmarks sandfs against the host filesystem and overlayfs. New capabilities land here.
- `sandfs/` is the Python package: re-exports from the `sandfs._core` extension, its type stubs (`_core.pyi`) and the `Source` protocol (`source.py`). Keep it thin; logic belongs in Rust.
- `tests/` contains the pytest suites for the Python surface; Rust tests live next to each crate (`crates/*/tests`).
- `docs/` tracks the roadmap, changelog and release steps (`docs/publishing.md`). Keep repository-wide guidance in README and this file.
- The version lives once, in the root `Cargo.toml` (`[workspace.package]`); the crates inherit it and maturin copies it into the Python package. `pyproject.toml` holds the rest of the package metadata.

## Build, Test, and Development Commands
- `uv pip install -e .[dev]` – build the extension with maturin and install sandfs in editable mode inside `.venv`. Re-run it after changing Rust code.
- `uv run pytest` – run the Python suite; required before every commit.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` – the Rust checks CI runs. The FUSE tests in `crates/sandfs-cli/tests/mount.rs` skip themselves without `/dev/fuse` and `fusermount3`; CI sets `SANDFS_REQUIRE_FUSE=1` so they cannot skip there.
- `uv run ruff check .` and `uv run ruff format sandfs tests` – lint and format Python.
- `uv run mypy sandfs` – type-check the package and its stubs; update `_core.pyi` whenever the extension's API changes.
- `cargo run --release -p sandfs-bench -- --scratch /dev/shm` – the benchmarks (build `sandfs-cli` in release too for the FUSE column); performance PRs quote its before and after numbers.
- `pre-commit install` – enables automatic Ruff and MyPy hooks before every commit.

## Coding Style & Naming Conventions
- Rust: edition 2024, `rustfmt.toml` settings, no `unsafe`, doc comments on public items. The one exception, approved by the maintainer, is `give_back` in `crates/sandfs/src/memory.rs`: one allocator call per platform that hands freed memory back to the system, allowed on that function alone (the workspace lint is `deny`, not `forbid`, for it). Don't add another without the maintainer's approval. Prefer small, composable functions over new concepts; a feature that can be built from mount, fork and changes should be.
- Python 3.11+, 4-space indentation, PEP 8 naming, type hints everywhere.
- Keep comments meaningful: a brief rationale before non-trivial logic. Avoid repository-specific jargon unless defined.

## Testing Guidelines
- Rust: integration tests in `crates/<crate>/tests`, unit tests beside the code. Python: pytest, files named `test_*.py`.
- Follow TDD: reproduce the agent scenario in a test before fixing code.
- Keep assertions specific (contents, versions, change kinds, exception types).

## Commit & Pull Request Guidelines
- Write commits in imperative present tense ("Fix version reuse after delete"). Each commit should include or reference updated tests.
- Pull requests should describe the behavior change, note any API changes, and mention how to run the new tests.
- Tag releases (`v0.x.y`) and let GitHub Actions build the wheels and publish via the trusted PyPI workflow; the tag must match the `Cargo.toml` version. The same tag publishes the crates to crates.io; `docs/publishing.md` has the one-time setup.

## Scope
- sandfs is a general-purpose sandbox filesystem: files are bytes of any kind, and code is one use case among many. Don't add features that only make sense for source code to the core; text tools belong in the MCP crate.
- It runs no commands of its own. The one exception is `sandfs shell`, which hands a program to the operating system's sandbox (bubblewrap, macOS's `sandbox-exec`) confined to a mount; the OS enforces it, and sandfs only sets it up. Code that may attack the kernel itself still belongs in a container or microVM pointed at the files.
- Sources are read-only; every change goes to the workspace overlay. Never add a path that writes back to a source implicitly.
