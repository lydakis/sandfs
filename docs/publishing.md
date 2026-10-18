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

## Release

1. Tag and push. Pushing a `v*` tag runs `release.yml`, which builds wheels for Linux (x86_64, aarch64, manylinux), macOS (x86_64, arm64) and Windows (x64) plus the sdist, smoke-tests the wheels, checks the tag matches the package version, and uploads to PyPI with trusted publishing (OIDC). PyPI needs a trusted publisher for `lydakis/sandfs` and `release.yml` before the first tag.

   ```bash
   git tag -a v0.4.0 -m "sandfs v0.4.0"
   git push origin v0.4.0
   ```

2. Publish the crates, core first because the others depend on it, and the CLI after the git source it uses:

   ```bash
   cargo publish -p sandfs-core
   cargo publish -p sandfs-git
   cargo publish -p sandfs-mcp
   cargo publish -p sandfs-cli
   ```

3. Create a GitHub release for the tag with the changelog entry as its notes.
