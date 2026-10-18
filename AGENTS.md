# Repository Guidelines

## Project Structure & Module Organization
- `crates/` holds the implementation. `sandfs-core` has the three primitives (sources, workspaces with a copy-on-write overlay, and the changes `diff()` reports) plus the directory and in-memory sources, `sandfs-git` is an optional lazy git source, and `sandfs-py` is the PyO3 extension, including sources written in Python. New capabilities land here.
- `sandfs/` is the Python package: re-exports from the `sandfs._core` extension, its type stubs (`_core.pyi`) and the `Source` protocol (`source.py`). Keep it thin; logic belongs in Rust.
- `tests/` contains the pytest suites for the Python surface; Rust tests live next to each crate (`crates/*/tests`).
- `docs/` tracks the roadmap and release notes. Keep repository-wide guidance in README and this file.
- `pyproject.toml` holds the package metadata and the version that drives the release workflow; update it before tagging.

## Build, Test, and Development Commands
- `uv pip install -e .[dev]` – build the extension with maturin and install sandfs in editable mode inside `.venv`. Re-run it after changing Rust code.
- `uv run pytest` – run the Python suite; required before every commit.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` – the Rust checks CI runs.
- `uv run ruff check .` and `uv run ruff format sandfs tests` – lint and format Python.
- `uv run mypy sandfs` – type-check the package and its stubs; update `_core.pyi` whenever the extension's API changes.
- `pre-commit install` – enables automatic Ruff and MyPy hooks before every commit.

## Coding Style & Naming Conventions
- Rust: edition 2024, `rustfmt.toml` settings, no `unsafe`, doc comments on public items. Prefer small, composable functions over new concepts; a feature that can be built from fork, mount and diff should be.
- Python 3.11+, 4-space indentation, PEP 8 naming, type hints everywhere.
- Keep comments meaningful: a brief rationale before non-trivial logic. Avoid repository-specific jargon unless defined.

## Testing Guidelines
- Rust: integration tests in `crates/<crate>/tests`, unit tests beside the code. Python: pytest, files named `test_*.py`.
- Follow TDD: reproduce the agent scenario in a test before fixing code.
- Keep assertions specific (contents, versions, change kinds, exception types).

## Commit & Pull Request Guidelines
- Write commits in imperative present tense ("Fix version reuse after delete"). Each commit should include or reference updated tests.
- Pull requests should describe the behavior change, note any API changes, and mention how to run the new tests.
- Tag releases (`v0.x.y`) and let GitHub Actions build the wheels and publish via the trusted PyPI workflow; keep `pyproject.toml` in sync with the tag.

## Scope
- sandfs is a general-purpose sandbox filesystem: files are bytes of any kind, and code is one use case among many. Don't add features that only make sense for source code to the core; text tools belong in the MCP crate.
- It never runs commands. Execution belongs to a container or microVM pointed at the files.
- Sources are read-only; every change goes to the workspace overlay. Never add a path that writes back to a source implicitly.
