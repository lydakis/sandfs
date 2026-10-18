# Repository Guidelines

## Project Structure & Module Organization
- `sandfs/` is the library core (VFS, node policies, shell, adapters, integrations). Start here when adding new capabilities or extending host fallback logic.
- `tests/` contains all pytest suites; mirror the runtime structure (e.g., `test_shell.py`, `test_vfs.py`) when adding scenarios.
- `crates/` holds the Rust core that replaces the Python VFS: `sandfs-core` (workspaces, sources, overlays, diff) and `sandfs-git` (lazy git mounts). New capabilities land here.
- `docs/` tracks the roadmap. Keep repository-wide guidance in README and this file.
- `pyproject.toml` holds metadata, dependencies, and version numbers that drive the release workflow; update it before tagging.

## Build, Test, and Development Commands
- `uv pip install -e .[dev]` – install sandfs in editable mode with dev extras inside the provided `.venv`.
- `uv run pytest` – run the full suite (currently ~110 tests) and is required before every commit.
- `uv run pytest tests/test_shell.py -k host` – focus on a single module/pattern when doing TDD.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` – the Rust checks CI runs.
- `uv run sandfs exec "ls /"` or `uv run sandfs shell --mount ./project:/workspace` – try commands from the CLI; inline examples (`python - <<'PY' …`) still work for API experiments.
- `uv run ruff check .` and `uv run ruff format sandfs tests` – lint (PEP 8, imports, bugbear, pyupgrade) and format.
- `uv run mypy sandfs` – run static type checks; keep hints up to date before committing.
- `pre-commit install` – enables automatic Ruff and MyPy hooks before every commit.

## Coding Style & Naming Conventions
- Python 3.11+, 4-space indentation, PEP 8 naming (snake_case for functions, PascalCase for classes). Type hints and dataclasses are expected; prefer `PurePosixPath` for paths.
- Keep comments meaningful (brief rationale before non-trivial logic, e.g., explaining policy enforcement). Avoid repository-specific jargon unless defined.
- When touching the shell, favor small helper methods to keep command handlers readable; add docstrings if the behavior is non-obvious.

## Testing Guidelines
- Framework: pytest only, with Hypothesis configured via dev extras. Test files are named `test_*.py`, and test functions start with `test_`.
- Follow TDD: reproduce the agent scenario in tests before fixing code. Examples include path translation regressions (`test_host_command_preserves_trailing_slash_paths`).
- Keep assertions specific (stdout text, versions, policies). Use fixtures/helpers (`setup_shell`) instead of duplicating VFS bootstrapping.

## Commit & Pull Request Guidelines
- Write commits in imperative present tense (“Fix host path rewrite…”). Each commit should include or reference updated tests.
- Pull requests (or internal reviews) should describe the behavior change, note any new commands/policies, and mention how to reproduce the new tests.
- Tag releases after meaningful fixes (`v0.x.y`) and let GitHub Actions publish via the trusted PyPI workflow; keep `pyproject.toml` version in sync with the tag.

## Agent & Host Integration Tips
- Host commands are not sandboxed: they run as the host user, and exist only with `SandboxShell(..., host_commands=True)` (or `host_fallback=True`). Host fallback materializes only what the shell's view may read under a temporary directory. Commands like `host -p /workspace …` or `bash -lc '…'` are the preferred way to use GNU utilities without re-implementing them.
- Always ensure writes stay inside the sandbox hierarchy. If you need host IO beyond that scope, add explicit adapters/policies rather than bypassing the VFS.
