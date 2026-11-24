# Changelog

## okr (development version)

### Bug fixes

- Remove the `generated` key from `okr.lock` (#84). It looked like the time
  the lock was written, but it never was: the lock is deterministic, so the
  key held midnight of `project.snapshot`, or `1970-01-01T00:00:00Z` for a
  lock without a snapshot, such as one resolved from `project.repo-url`.
  The lock now records no time. The snapshot date is still recorded as
  `snapshot`.

  The lock format version is now 3. `okr sync` regenerates a version 2 lock
  with a warning, while `okr status` and `okr verify` ask for `okr sync`.
  The environment digest hashes the lock format version, so the regenerated
  lock and `_manifest.json` carry a new one even when every locked source and
  tree digest is unchanged.

## okr 0.1.13

### Breaking changes

- `project.repo-url` is now the complete URL of the repository that CRAN
  packages are resolved from, and it is used exactly as written (#81).
  It used to be a base URL to which `okr` appended `project.snapshot`, so
  a repository could only be used if its URL ended in a `YYYY-MM-DD` date.
  Any CRAN-like repository now works, whatever its URL looks like: a Package
  Manager snapshot with another identifier, a Package Manager URL for a Linux
  distribution, a `latest` alias, a CRAN mirror, or a local `file://`
  repository. For example:

  ```toml
  [project]
  repo-url = "https://ppm.example.com/cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7"
  ```

  `snapshot` and `repo-url` are now mutually exclusive. A configuration that
  sets both fails with exit code 2, and the error names the `repo-url` to
  write instead: `snapshot = "2026-06-30"` with
  `repo-url = "https://ppm.example.com/cran"` becomes
  `repo-url = "https://ppm.example.com/cran/2026-06-30"` alone. The next
  `okr sync` rewrites the lock without `snapshot` and with `generated` at the
  Unix epoch. The locked URLs, tree digests, and environment digest stay the
  same.

  Configurations that set only `snapshot` are not affected. `snapshot` still
  selects the dated Posit (Public) Package Manager CRAN snapshot, and the lock
  format is unchanged.
- Validate `repo-url` when the configuration is read (#81). It must be an
  `https://`, `http://`, or `file://` URL of the repository root, without
  whitespace, a query, a fragment, or a trailing `src/contrib`.

### Improvements

- Read the package index of a `repo-url` repository again whenever an online
  `okr sync` resolves sources, because `okr` cannot tell whether such a
  repository is a fixed snapshot (#81). A repository that moves, such as a
  CRAN mirror, no longer resolves against a stale cached index. The cached
  copy is still used with `--offline`, and the index of a dated `snapshot` is
  still cached.
- Warn when a `"*"` package resolves to a different version than an
  up-to-date lock recorded, which happens when the repository has moved since
  the last sync (#81). The new version is locked, as for a branch ref.
- Read the repository in the companion installation command from the lock, so
  that the command keeps installing the vendored versions after `okr.toml`
  has changed (#81).
- Point an invalid `project.snapshot` value, such as `latest` or a snapshot
  identifier that is not a date, to `project.repo-url` (#81).

### Bug fixes

- Report a missing file behind a `file://` URL as a fetch error with exit
  code 3 that names the file, instead of a bare I/O error with exit
  code 1 (#81).

### Dependencies

- Adopt a 7-day dependency cooldown for development with Cargo 1.100+ (#79).

### Testing

- Fix an intermittent `ETXTBSY` failure in the fake `Rscript` unit tests on
  Linux by writing the script from a child process (#79).

## okr 0.1.12

### Improvements

- Redesign the interactive `okr sync` progress display so that each source
  reports its own progress (#75). A header names the stage and counts
  finished sources. The row beneath it follows the source being worked on and
  shows either the step, such as `resolving v1.5.0 via git ls-remote (3s)`,
  or a download with its bytes, rate, and host. Each stage ends in one summary
  line that stays on screen, such as
  `Vendored 12 sources in 14.2s · 38.4 MiB downloaded`.

  Work that finishes within 200 ms is not drawn, and a finished row is reused
  rather than removed. A sync served from the cache therefore no longer
  flashes messages or shifts lines.
- Keep one line per source in each stage under `--verbose`, with the resolved
  version or commit, the fetch method, and the download size (#75).
  These lines are also written when standard error is not a terminal.
  `--verbose` no longer turns the interactive display off. Without `--verbose`,
  redirected output still carries no progress.

### Bug fixes

- Stop `git` from writing to the terminal during `okr sync` (#75).
  Clones echoed their `$ git ...` command lines, even under `--quiet`,
  and a failing `git ls-remote` printed its error before `okr` fell back to
  a forge API. Both overwrote the progress display and left stale lines behind.
  A failing `git` command now reports its own message as part of the `okr` error.

## okr 0.1.11

### Dependencies

- Upgrade `zip` from 8.6.0 to 9.2.1 (#72). The upgrade adds `nix` as a
  transitive dependency on macOS and BSD systems.

  Archives with ASCII entry names or names flagged as UTF-8, including
  the existing fixtures, extract to identical trees and locked tree digests.
  zip 9 improves on two edge cases: unflagged names that are valid UTF-8 are
  now decoded as UTF-8 rather than CP437, and the Unix mode bits in entries
  created on MS-DOS hosts are ignored in favor of the DOS attributes.

## okr 0.1.10

### New features

- Accept `.zip` archives as direct `url::` sources under `[packages]` and
  `[references]` (#68). For example, a CTAN or release zip archive can be
  now declared as `{ url = "https://.../example.zip", sha256 = "..." }`.
  The archive format is detected from the downloaded bytes, and zip entries
  pass through the same wrapper directory, path safety, and symbolic link
  rules as tar entries. The `sha256` pin and the `tarball` fetch method are
  unchanged, so existing lockfiles remain valid.

### Dependencies

- Add `zip` with default features disabled and only `deflate-flate2` enabled,
  so zip decompression reuses the existing `flate2` dependency and adds
  `typed-path` as the only new transitive crate (#68).

## okr 0.1.9

### Improvements

- Keep only the digests that have a consumer in `okr.lock` (#64).
  The per-entry `artifact-digest` is removed, leaving `config-digest`,
  `environment-digest`, and one `tree-digest` per entry.

  Sources remain identified by snapshot and version, full commit, or a declared
  `sha256` pin. Offline replay finds cached artifacts by fetch method and
  source; and a tree rebuilt from a fresh lock must reproduce its locked tree digest.

  The lock format version is now 2 and `_manifest.json` uses schema 2 without
  `artifact_digest`. `okr sync` regenerates a version 1 lock with a warning,
  while `okr status` and `okr verify` ask for `okr sync`.

## okr 0.1.8

### Improvements

- Avoid editing `AGENTS.md` by default. The managed okr marker block remains
  available by explicitly setting `manifest.agents-file = true` in `okr.toml`.
  Existing configurations that already opt in keep their current behavior (#60).

## okr 0.1.7

### Bug fixes

- Allow Git repositories to contain symbolic links by safely materializing
  each link as a regular file containing its exact target bytes, consistently
  across forge archives, git clones, and offline cache replay (#56).

## okr 0.1.6

### Improvements

- Explain when `@*release` cannot find a published GitHub release, including
  how to use a repository that has only Git tags, without printing the `gh`
  authentication probe or every failed API tier (#53).

## okr 0.1.5

### Improvements

- `okr init` now adds anchored, escaped rules to existing `.Rbuildignore`
  to keep the vendored source tree, `okr.toml`, and `okr.lock` out of
  R package tarballs (#50).

### Documentation

- Document installation via Homebrew tap as an alternative to `cargo install` (#44).
- Remove system requirement discovery and Dockerfile emission from the roadmap.
  Delegate those concerns to renv, pak, and container tooling. Document
  okr's boundary as composable source context artifacts and verification (#43).

## okr 0.1.4

### Improvements

- Remove the default comment for the `project.snapshot` field from generated
  `okr.toml`, and document how to select a specific dated snapshot (#40).
- Remove the unused `project.name` configuration field from `okr.toml`.
  Define `project.r-version` as an optional exact expectation for the project
  harness's R runtime, validate its `major.minor.patch` form, and initialize it
  from the project's current `Rscript` when available instead of emitting a
  placeholder value (#39).
- Explain clearly when a synced source declared in `[packages]` has no
  `DESCRIPTION` file, including how to move or re-add a non-R project
  under `[references]` (#38).

## okr 0.1.3

### Improvements

- Standardize generated integrity fields on subject-based digest names and
  `sha256:<hex>` values: `config-digest`, `environment-digest`, `artifact-digest`,
  and `tree-digest` in TOML, with snake case equivalents in JSON (#30).

### Dependencies

- Remove the redundant `proptest` development dependency and disable unused
  `insta` JSON and redaction features (#32).

## okr 0.1.2

### Bug fixes

- Inspect installed packages with normal R startup semantics so customized
  system, user, and project library paths are honored (#20).

### Improvements

- Initialize `project.snapshot` to the latest available dated CRAN snapshot,
  searching backward from the current UTC date, so the first bare
  `okr add` succeeds without a manual configuration edit (#24).
- Show interactive `okr sync` progress with an overall entry counter,
  per-source spinners, and byte-level download bars, while keeping quiet,
  verbose, and non-terminal output stable and uncluttered (#23).
- Replace serialized per-file hash inventories in lockfiles and JSON manifests
  with aggregate tree digests, to keep generated metadata compact (#19).
- Keep environment digests out of routine `okr sync` output unless `--verbose`
  is requested (#20).

## okr 0.1.1

### Bug fixes

- Accept repeated same-version package records in snapshot `PACKAGES` indexes,
  as emitted for R's recommended packages by Posit (Public) Package Manager (#11).
- Deterministically choose the greatest R package version when repeated
  snapshot index records disagree (#13).
- Ignore PAX global metadata headers when determining and stripping a source
  archive's top-level directory, including archives generated by GitHub (#11).

### Dependencies

- Use full version requirements for Cargo dependencies (#10).

## okr 0.1.0

### New features

- The initial `okr` CLI with `init`, format-preserving `add`, idempotent
  `sync`, diagnostic `status`, and integrity-gating `verify` commands (#3).
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
