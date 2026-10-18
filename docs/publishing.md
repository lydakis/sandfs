# Publishing sandfs

The `sandfs` package on PyPI is a Rust extension built with [maturin](https://www.maturin.rs). Each release ships one abi3 wheel per platform, which covers every Python from 3.11 up, plus an sdist that builds from source when a Rust toolchain is installed.

## Release steps

1. Set `version` in `pyproject.toml` to the release number.
2. Check the build locally:

   ```bash
   uv build          # sdist and a wheel for this machine, via maturin
   ```

3. Tag and push:

   ```bash
   git tag -a v0.4.0 -m "sandfs v0.4.0"
   git push origin v0.4.0
   ```

## GitHub Actions release

Pushing a `v*` tag runs `.github/workflows/release.yml`:

1. `PyO3/maturin-action` builds wheels for Linux (x86_64, aarch64, manylinux), macOS (x86_64, arm64) and Windows (x64), plus the sdist.
2. `pypa/gh-action-pypi-publish` uploads them with PyPI trusted publishing (OIDC), so no API token is needed once the trusted publisher for `lydakis/sandfs` and `release.yml` is configured on PyPI.
