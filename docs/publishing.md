# Publishing sandfs

A release ships the `sandfs` Python package to PyPI and the `sandfs-core`, `sandfs-git`, `sandfs-mcp` and `sandfs-cli` crates to crates.io, all at one version number. `sandfs-py` is the extension crate inside the Python package and is not published on its own.

The Python package is built with [maturin](https://www.maturin.rs): one abi3 wheel per platform covers every Python from 3.11 up, plus an sdist that builds from source when a Rust toolchain is installed.

## Version

The version lives in one place, `version` under `[workspace.package]` in the root `Cargo.toml`. The crates inherit it and maturin copies it into the Python package. Bump it together with the `version` of the `sandfs-*` entries under `[workspace.dependencies]`, and move the changelog entry from "unreleased" to the release date.

## Checks before tagging

Every pull request already runs these in CI (`ci.yml` and `release.yml`). To run them by hand:

```bash
uv build                                       # sdist, then a wheel built from it
cargo package --workspace --exclude sandfs-py  # builds the crates as crates.io would
```

## One-time setup

- **PyPI**: the `sandfs` project has a trusted publisher for repository `lydakis/sandfs`, workflow `release.yml`, no environment. Releases up to 0.2.2 were uploaded through it.
- **crates.io**: an API token with the `publish-new` and `publish-update` scopes, saved as the repository secret `CARGO_REGISTRY_TOKEN` (Settings → Secrets and variables → Actions). Without it the crates job publishes nothing and says so in a warning.

## Release

1. Tag and push:

   ```bash
   git tag -a v0.5.0 -m "sandfs v0.5.0"
   git push origin v0.5.0
   ```

   The tag runs `release.yml`. It builds wheels for Linux (x86_64, aarch64, manylinux), macOS (x86_64, arm64) and Windows (x64) plus the sdist, smoke-tests the wheels, checks the tag matches the package version, and uploads to PyPI. In parallel it publishes every crate without `publish = false` to crates.io, dependencies first, and builds the `sandfs` command for Linux and macOS. Once everything has built, it attaches those builds to the tag's GitHub release. If no release exists for the tag yet, it first creates one, using the changelog entry as its notes.

2. If the crates job published nothing or stopped partway (no token yet, or a network error), run the workflow by hand on the tag (Actions → Release → Run workflow, with the tag selected). It skips crates that crates.io already has at that version.

3. To write the release notes yourself, create the GitHub release instead of pushing the tag (`gh release create vX.Y.Z --target main --notes-file NOTES`). That also creates the tag, which runs the workflow as above, and the workflow then only attaches the builds.
