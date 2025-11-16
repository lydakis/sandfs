# Publishing sandfs

1. Ensure `pyproject.toml` has the correct `version` and changelog/README are up to date.
2. Build the package:

```bash
uv build
```

This produces distributions under `dist/`.

3. Publish to PyPI:

```bash
uv publish --username <pypi-user>
```

`uv publish` will prompt for credentials or can read `PYPI_TOKEN`. Remove `--dry-run`; uv currently publishes directly.

If you prefer Twine:

```bash
python -m build
python -m twine upload dist/*
```

4. Tag the release:

```bash
git tag -a v0.1.0 -m "sandfs v0.1.0"
git push origin v0.1.0
```

## GitHub Actions release

Once PyPI is configured with a trusted publisher for `lydakis/sandfs` (workflow `release.yml`), pushing a `v*` tag triggers `.github/workflows/release.yml`. The workflow:

1. Builds artifacts via `uv build`.
2. Uses `pypa/gh-action-pypi-publish@release/v1` with OIDC credentials to upload the contents of `dist/`.

No API tokens are needed once the trusted publisher is authorized.
