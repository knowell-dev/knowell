# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project will adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once 1.0
is released. Nothing has been released yet.

## [Unreleased]

### Fixed

- Keep worktree identities stable across Windows short and long path spellings.
- Test invalid UTF-8 path handling on Unix without requiring filesystem support for
  invalid filenames; retain the Linux filesystem regression test.
- Resolve test binaries and fixture paths at runtime when CI relocates a Nextest archive.
- Fix CLI help markup and ambiguous links that failed Rustdoc with warnings denied.
- Preserve executable Action test shims and reject unexpected network requests in the
  offline harness; make the OIDC prerequisite check explicit for shellcheck.

### Added

- Initial Rust workspace, contribution rules, and project documentation.
- Continuous integration: formatting, linting, dependency policy, secret scanning,
  workflow security checks, tests, and a small synthetic evaluation.
