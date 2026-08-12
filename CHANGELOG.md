# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-11

### Added

- The initial `okr` CLI with `init`, format-preserving `add`, idempotent
  `sync`, diagnostic `status`, and integrity-gating `verify` commands.
- Strict `okr.toml` support for `[packages]` and `[references]`, including
  string and table entries, per-entry pruning options, URL SHA-256 pins, and
  hard errors for unknown keys.
- The milestone 0.1 R `Remotes` grammar: CRAN snapshot entries, GitHub,
  GitLab, Bitbucket, arbitrary `git::` sources, verified `url::` tarballs,
  named refs, full commit pins, and GitHub `@*release` resolution.
- Instructive deferrals for Bioconductor and local sources to milestone 0.2,
  pull-request refs to milestone 0.3, and permanent rejection guidance for
  SVN sources.
- Solver-free CRAN resolution through dated `PACKAGES.gz` metadata, including
  explicit-pin validation and CRAN archive fallback URLs.
- Tiered GitHub access through authenticated `gh`, `GITHUB_TOKEN`, and
  anonymous REST, with public-forge archive downloads and shallow git-clone
  fallback through the user's existing credentials.
- A SHA-256 content-addressed cache with verified hits, atomic writes,
  normalized clone-tree archives, `OKR_CACHE_DIR`, and complete `--offline`
  reconstruction support.
- Deterministic package/reference vendoring with safe tar extraction,
  top-level directory stripping, kind-specific pruning, user exclude globs,
  `core.autocrlf=false` clones, and atomic vendor-root replacement.
- Stable `okr.lock` generation with config hashes, per-file inventories,
  tree digests, artifact hashes, fetch-method provenance, and a reproducible
  environment digest.
- Full-tree and generated-manifest verification, including schema-versioned
  JSON mismatch reports and exit code 4 for any integrity drift.
- Schema-versioned `_manifest.json`, separate package/reference tables in
  `_manifest.md`, marker-delimited `AGENTS.md` management, and idempotent
  `.gitignore` management.
- Read-only installed-R-library inspection. Missing R is reported as a
  successful skip; version mismatches warn by default and fail under
  `--strict` or `project.strict` while printing, but never executing, a
  companion installation command.
- Deterministic local fixtures and coverage for offline rebuilds, no-op syncs,
  file-based git remotes, deliberate tree mutation, strict coherence, and
  byte-identical lock and manifest regeneration without live network access.
- Stable Rust CI on Ubuntu and macOS with formatting, warnings-as-errors
  clippy, and test gates; crates.io metadata and dual MIT/Apache-2.0 licensing.

[Unreleased]: https://github.com/nanxstats/okr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nanxstats/okr/releases/tag/v0.1.0
