# knowell (PyPI launcher) -- scaffold, NOT published

Status: **not published**. Whether Knowell gets a PyPI package is undecided, so this directory
is a tested scaffold only and the release workflow's PyPI job is disabled.

If published, it mirrors the npm launcher (`uvx knowell`, `pipx install knowell`): no build
step and no dependencies; on first run it downloads the release binary for the platform from
GitHub Releases, verifies it against `SHA256SUMS`, caches it and runs it with the terminal
attached. The package version is the Knowell version it runs. The command is `know`.

Environment: `KNOWELL_BIN`, `KNOWELL_BINARY_VERSION`, `KNOWELL_CACHE_DIR`,
`KNOWELL_DOWNLOAD_BASE` (same meaning as in `wrappers/npm`).

Tests (offline): `python -m unittest discover -s wrappers/pypi/tests` from the repository root,
or from this directory: `PYTHONPATH=src python -m unittest discover -s tests`.
