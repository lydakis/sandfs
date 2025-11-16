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
