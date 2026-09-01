# AGENTS.md

This file is guidance for coding agents working on `okr` itself. `SPEC.md` is
the source of truth for behavior, schemas, grammar, naming, and milestone
scope. Read it in full before changing behavior; if this file disagrees with it,
follow `SPEC.md`.

## Project boundary

`okr` is a Rust CLI that retrieves, organizes, and attests R package sources
and arbitrary reference repositories for coding agent context. It is not an R
package manager.

Keep these invariants intact:

- `okr` never installs R, installs packages, or writes to an R library. R
  inspection is read-only and optional.
- CRAN resolution is a lookup in the dated snapshot's `PACKAGES.gz`; there is
  no dependency or version-constraint solver.
- Runtime HTTP is synchronous. Do not add an async runtime.
- Do not add libgit2, gix, or credential storage. Invoke optional `git` and
  `gh` binaries through `xshell` with explicit argument vectors and inherited
  environment variables.
- `--offline` must not download, query an API, run `git ls-remote`, or clone.
- Unknown `okr.toml` keys remain hard errors. Config writes performed by
  `okr add` must use `toml_edit` and preserve user comments and formatting.
- Tree drift is always fatal to `verify` with exit code 4. Installed-library
  drift warns by default and becomes fatal only under `--strict` or
  `project.strict`.
- System-requirement discovery and Dockerfile emission are out of scope. Point
  users to `renv::sysreqs()` / `pak::pkg_sysreqs()` and their chosen container
  tooling instead.
- Milestone 0.2 features belong on the roadmap until their milestone
  is intentionally started. Do not partially introduce profiles, bundles,
  Bioconductor/local sources, PR refs, or transitive resolution.

## Runtime architecture

The main convergence path is:

1. `cli` loads `config` and parses declarations through `spec`.
2. `resolve` freezes CRAN versions and remote refs into exact source plans.
3. `fetch` and `hosttools` acquire verified artifacts; `vendor` extracts or
   clones, prunes, and atomically replaces the vendor tree.
4. `digest` inventories the resulting bytes; `lock` records provenance and
   integrity; `manifest` writes agent-facing indexes and discovery files.
5. `rlib` optionally compares the locked package versions with the installed
   R library without mutating it.

`src/main.rs` stays thin: parse the CLI, call the library, render the error,
and map it to the documented exit code. Application behavior belongs in the
library modules.

## Module map

| Module | Responsibility | Important constraints |
|---|---|---|
| `src/main.rs` | Binary entry point and `anyhow` context at the process boundary. | Keep it thin; delegate behavior to the library and preserve library exit-code mappings. |
| `src/lib.rs` | Public modules, shared `Error`, and stable exit-code classes. | Config/spec = 2, fetch = 3, verification/coherence = 4, unexpected I/O = 1. |
| `src/cli.rs` | Clap surface and orchestration for `init`, `add`, `sync`, `status`, and `verify`. | Keep `main.rs` thin; reject `--json` where unsupported; preserve sync's digest-based no-op path. |
| `src/config.rs` | Strict serde models, defaults, validation, and declaration normalization for `okr.toml`. | Keep `deny_unknown_fields`, safe relative vendor paths, URL digests, and package/reference name collision checks. Never use serde to write `okr add` edits. |
| `src/spec.rs` | Parser for the supported R `Remotes` grammar and CRAN/remote disambiguation. | Preserve instructive 0.2/rejected-source errors. Extend its table-driven and hostile-input tests with every grammar change. |
| `src/resolve.rs` | Snapshot lookup, archive fallback, git-ref freezing, public-forge API fallback, and GitHub release tiering. | Lookup only, never solve constraints. Full 40-character SHAs skip ref resolution. Keep the `GithubReleaseApi` seam stubbable. |
| `src/resolve/dcf.rs` | Minimal DCF parser for `PACKAGES` and `DESCRIPTION`. | Support continuation lines and multiple stanzas without adding a parsing dependency. |
| `src/fetch.rs` | Synchronous HTTP/file acquisition and the content-addressed cache under `OKR_CACHE_DIR`. | Verify declared SHA-256 pins before commit, use temp-file plus rename, reverify hits against their content address, index every artifact by its source key for digest-free replay, and fail clearly on offline misses. |
| `src/hosttools.rs` | Optional `git`/`gh` discovery and all host-tool subprocess calls. | Use only `xshell` argument vectors; inherit user auth; produce actionable missing-tool errors; clone with `core.autocrlf=false`. |
| `src/vendor.rs` | Tar extraction, clone fallback, kind-specific pruning, metadata inspection, normalized clone caching, and atomic tree replacement. | Reject unsafe tar entries and special files; safely normalize source-controlled symbolic links; packages and references have different pruning defaults; record the actual fetch method; a tree rebuilt from a fresh prior lock must reproduce its locked tree digest. |
| `src/digest.rs` | SHA-256 helpers and deterministic source tree inventories. | Sort paths, normalize separators to `/`, hash file bytes exactly, and reject symlinks/non-files. |
| `src/lock.rs` | Stable lock construction/serialization plus full vendor and manifest verification. | Sort entries, retain aggregate tree digests, recompute the environment digest, and keep clean rebuilds byte-identical. Bump `LOCK_VERSION` for any schema change and keep `sync` able to regenerate older locks. |
| `src/manifest.rs` | `_manifest.json`, `_manifest.md`, `AGENTS.md` marker blocks, and managed `.gitignore` entries. | JSON is schema-versioned through `MANIFEST_SCHEMA`; bump it when an entry field is added or removed; package/reference sections remain distinct; only text inside okr marker blocks may be replaced. |
| `src/rlib.rs` | Read-only `Rscript` discovery, installed package enumeration, and coherence comparison. | Absence of R is a successful skip with a note. Never execute the companion install command. |

## Determinism and provenance

Deterministic output is a correctness requirement, not a cosmetic preference.

- Use ordered collections for serialized or hashed data and sort filesystem
  inventories explicitly.
- Tree inventory records are `path<TAB>file-sha256`, joined with LF and no
  trailing newline. Paths are UTF-8 and `/`-normalized.
- The lock's `generated` value is snapshot midnight, or the Unix epoch for a
  remote-only lock; do not replace it with wall-clock time.
- Per-file hashes are an internal input to each aggregate tree digest; do not
  serialize the inventory in the lock or manifest. The aggregate tree digests
  are part of the environment digest.
- The tree digest is the only per-entry digest. Do not reintroduce an
  artifact digest: a source is identified by snapshot, version, and URL, by
  its full commit, or by a declared `sha256` pin, and the cache is indexed by
  fetch method and source for replay.
- Clone-produced trees are cached as normalized gzip tarballs with sorted
  paths, zero mtimes and ownership, and normalized modes.
- `fetch-method` is provenance. Reproduction should replay the locked method
  through its own cache key instead of silently switching between forge
  archives and clones, whose `.gitattributes` export behavior may differ.
- Text fixtures are LF-normalized by the repository `.gitattributes`; binary
  fixture archives must remain byte-for-byte unchanged.

## Tests and fixtures

- Unit tests live beside their modules. Grammar tests in `spec.rs` should
  remain the most exhaustive parser suite.
- CLI integration tests are in `tests/cli_project.rs` and
  `tests/cli_sync.rs`.
- Acquisition tests must remain local. Reusable source inputs live under
  `tests/fixtures/`; never add a test that requires live network access.
- Git tests create temporary repositories with `git init` and access them via
  `file://`. Skip only when a test genuinely exercises the optional host git
  binary.
- Lock and manifest formats are covered by `insta` snapshots under
  `src/snapshots/`. Review snapshot diffs as schema changes, not incidental
  updates.
- Tests that simulate R should place a fake, read-only `Rscript` on `PATH` and
  assert warning-versus-strict behavior.

## Working conventions

Before editing, inspect `git status` and preserve unrelated user changes.
Prefer small changes in the owning module and add regression coverage at the
same layer, followed by an end-to-end test when command behavior changes.
Avoid new dependencies; if one is necessary beyond the list in `SPEC.md`
section 15, document the reason next to it in `Cargo.toml`.

Run the release gates before handing off:

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo publish --dry-run
```

Use conventional commits. Update `CHANGELOG.md` for user-visible changes and
keep the README roadmap limited to the milestones in `SPEC.md` section 16.
