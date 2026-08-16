# okr design specification

`okr` gives coding agents readable R package sources and records exactly which
sources they received. It builds a vendored source tree, describes that tree in
agent-facing manifests, and attests its contents with deterministic digests.

## 1. Make installed code readable

An installed R package is designed to run, not to be read. Its R code is stored
in binary lazy-load databases (`.rdb` and `.rdx`), and source files compiled
from `src/` are not included in the installed package. A coding agent inspecting
an R library can usually read `DESCRIPTION` and `NAMESPACE`, but it can't
inspect the original implementation via regular text search.

Model evaluation environments add a second requirement. They need exact source
trees that can be staged before the network is disabled and checked before an
eval run starts. A stable digest lets the harness confirm that every run
received the same inputs and that those inputs did not change.

`okr` addresses both needs with one artifact: a vendored tree containing the
declared R packages and reference repositories. `okr.toml` declares the desired
sources, `okr.lock` records the resolved sources and their provenance, and
`okr verify` checks the resulting bytes.

## 2. Keep responsibility narrow

`okr` retrieves, organizes, and attests source context. It never installs R,
installs R packages, or writes to an R library. Those tasks already have
dedicated tools:

| Task | Owner |
|---|---|
| Install R | `rig` |
| Install packages and resolve dependencies | `pak`, `renv`, `rv`, or `install.packages()` |
| Provide dated CRAN snapshots | Posit (Public) Package Manager |
| Discover system requirements | `renv::sysreqs()` or `pak::pkg_sysreqs()` |
| Construct container images | `renv` and the user's container tooling |
| Provide readable source context and attest it | `okr` |

When R is available, `okr` may inspect the installed library as a read-only
diagnostic. It also prints a companion installation command that uses the same
snapshot. It never runs that command.

## 3. Goals

1. Vendor the exact source of each declared R package and reference repository.
2. Accept the familiar subset of the R `Remotes` grammar defined in §7.
3. Require the vendored package version to match the resolved version.
   Report differences between that version and the installed R library
   without making them fatal by default.
4. Record compact, aggregate content digests that support byte-level
   verification and provide one environment digest for evaluation runs.
5. Generate manifests that make the vendored tree easy for coding agents to
   discover and navigate.
6. Support offline use from a local cache in 0.1, with portable bundles planned
   for 0.2.
7. Work with any Git host supported by the user's `git` installation, including
   public forges, GitHub Enterprise, self-hosted GitLab, `git://` remotes, and
   SSH remotes. Reuse the user's existing Git and `gh` authentication instead of
   storing credentials.

## 4. Leave other jobs to other tools

- **Keep installation external.** `okr` does not install R or R packages and
  does not modify an R library.
- **Resolve by lookup.** CRAN resolution reads the selected snapshot's
  `PACKAGES` index; it does not solve dependency constraints.
- **Supply context, not evaluations.** Task definitions, agent loops, and
  grading belong to the evaluation harness. `okr` supplies machine-readable
  inputs to that harness.
- **Reuse existing authentication.** GitHub authentication comes from `gh` or
  `GITHUB_TOKEN`; Git authentication comes from the user's Git configuration
  and environment.
- **Stay focused on R.** `okr` focuses on addressing a property of installed
  R packages; it is not designed as a language-agnostic source vendoring tool.
- **Keep package manager lockfiles separate.** No `renv` or `rv` lockfile conversion.
- **Leave runtime construction external.** System requirements and Dockerfiles
  describe the runtime rather than the source context.
  Use `renv::sysreqs()` or `pak::pkg_sysreqs()` for system packages and the
  user's preferred container tooling for images. The container-facing interface
  of `okr` is its lockfile, vendor tree, manifests, verification command, and,
  in 0.2, its portable bundle.

## 5. Glossary

- **`okr.toml`** is the committed project configuration.
- **`okr.lock`** is the committed record of resolved versions, commits, URLs,
  fetch methods, and integrity digests.
- **Vendor tree** is the generated source tree, `deps-src/` by default, with one
  directory per declared entry.
- **Package** is an R package vendored at one exact version. It participates in
  source version checks and installed library diagnostics.
- **Reference** is any other repository included as read-only context:
  for example, a standards repository, protocol templates, or a non-R codebase.
  It has no package-version semantics and does not participate in R library
  diagnostics. Its commit is still pinned.
- **Snapshot** is a dated PPM repository such as
  `https://packagemanager.posit.co/cran/2026-06-30`. It is required when, and
  only when, the configuration contains a CRAN package.
- **Cache** is the content-addressed artifact cache under `$OKR_CACHE_DIR`, or
  `~/.cache/okr` by default. Artifacts are keyed by SHA-256.
- **Manifest** means `deps-src/_manifest.json` and `deps-src/_manifest.md`.
  R package names begin with a letter, so these underscore-prefixed files
  cannot collide with package directories.
- **Profile** is a curated `okr.toml` template selected by `okr init`.
  Profiles are planned for 0.2.
- **Bundle** is a deterministic archive containing the configuration, lockfile,
  vendor tree, and optionally the artifacts needed for offline reconstruction.
  Bundles are planned for 0.2.

## 6. Design a small command line

The command surface in 0.1 is:

```text
okr init [--force]
okr add <spec>... [--reference]
okr sync [--offline] [--strict]
okr status [--json]
okr verify [--json] [--strict]

Shared options: --config <path>  --quiet  --verbose
```

`--json` is available only on the commands that show it above. Supplying it to
another command is an error.

The 0.2 roadmap adds:

```text
okr init --profile <name-or-path-or-url> [--force]
okr bundle [-o <path>] [--include-cache]
```

### `init`

`okr init` writes `okr.toml`. For the default template, it looks for a PPM
snapshot by requesting `PACKAGES.gz` for the current UTC date and then walking
back at most 14 days. It writes the first available date to
`project.snapshot` and caches the successful response for the first sync.

Only a response that means "snapshot not found" advances to the preceding
date. Connectivity failures and server errors are fetch errors.
`init` never writes the moving `latest` alias because a configuration must
identify a stable snapshot.

If `Rscript` is on `PATH`, `init` runs it read-only from the project directory
and records its exact `major.minor.patch` version as `project.r-version`.
If R is absent or cannot be inspected, it omits this optional field.
`init` refuses to replace an existing configuration unless `--force` is supplied
and offers to add the vendor path to `.gitignore` as described in §12.

In 0.2, `--profile` uses a profile template instead of the default template.

### `add`

`okr add` parses every argument with the grammar in §7 and adds it to
`[packages]`, or to `[references]` when `--reference` is present.
It uses `toml_edit`, so comments and formatting outside the edit are preserved.
It does not run `sync`.

### `sync`

`okr sync` makes the generated state agree with `okr.toml`.
It is idempotent and has five stages:

1. **Resolve.** Freeze every package version and remote ref into an exact
   source plan (§8).
2. **Fetch and vendor.** Acquire verified artifacts, extract or clone them,
   prune them, and atomically replace their vendor directories (§9).
3. **Lock.** Write `okr.lock` (§11).
4. **Describe.** Write both manifests and, when enabled, update the managed
   block in `AGENTS.md` (§12).
5. **Compare.** If R is available, compare the installed package versions with
   the vendored versions. Report drift and print the companion installation
   command. Under `--strict` or `project.strict = true`, drift exits with
   code 4 (§10).

When the lock is current and the complete vendor tree matches its digests,
`sync` takes a digest-based no-op path.

### `status`

`okr status` reports:

- whether R is available and which version it exposes;
- whether that version matches `project.r-version`, when declared;
- whether the configuration digest matches the lock;
- a spot-check of vendor tree integrity;
- a summary of installed library coherence;
- cache statistics; and
- a copyable companion installation command.

When `project.strict = true`, installed library drift makes `status` exit 4.
There is no command-specific `--strict` flag for `status`.

For example:

```text
install with:  Rscript -e 'pak::pkg_install(c("rpact","gsDesign"), repos="https://packagemanager.posit.co/cran/2026-06-30")'
```

### `verify`

`okr verify` hashes the complete vendor tree and compares it with `okr.lock`.
Tree drift is always fatal: a clean tree exits 0 and any drift exits 4. With
`--json`, the report lists each mismatched entry and its expected and actual
aggregate tree digests.

`--strict`, or `project.strict = true`, also makes installed library drift
fatal. This is useful when an evaluation treats the installed R library as part
of the attested environment.

### `bundle` (0.2)

`okr bundle` writes a deterministic `tar.zst` containing `okr.toml`,
`okr.lock`, and the vendor tree. `--include-cache` also includes the artifacts
needed to rebuild that tree. Entries are sorted by path, mtimes and ownership
are zeroed, and modes are normalized to 0644 or 0755. The command prints the
bundle's SHA-256, which must be reproducible across machines.

### Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Unexpected I/O or internal error |
| 2 | Configuration or source-specification error |
| 3 | Network, resolution, or fetch error |
| 4 | Verification or strict installed library coherence failure |

## 7. Use familiar source specifications

`okr` supports a deliberate subset of the syntax used by the R `Remotes:` field.
The same syntax is accepted by `okr add` and by string values in `okr.toml`:

```text
spec        := [type "::"] body ["@" ref]
type        := "github" | "gitlab" | "bitbucket" | "git" | "url"
body        := owner "/" repo            (forge types; github is the default)
             | <Git URL>                 (`git::`; any protocol Git supports)
             | <HTTP(S) tarball URL>     (`url::`)
ref         := tag | branch | commit SHA | "*release"   (GitHub only)
```

| Specification | Meaning |
|---|---|
| `pharmaverse/admiral@v1.5.0` | GitHub tag; `github::` is implicit |
| `github::tidyverse/ggplot2` | GitHub default branch; warn and lock the resolved SHA |
| `r-lib/testthat@*release` | Latest GitHub release; resolve it and lock its SHA |
| `gitlab::jimhester/covr@abc123` | Commit or named ref on gitlab.com |
| `bitbucket::sulab/mygene.r@default` | Branch on bitbucket.org |
| `git::git@ghe.corp.example:stats/simlib.git@v2.1` | Any Git host, using the user's Git authentication |
| `git::https://codeberg.org/org/pkg.git@v1.0` | Explicit Git URL over HTTPS |
| `url::https://example.com/pkg_0.2.1.tar.gz` | Direct tarball; a `sha256` pin is required in table form |

The following `Remotes` forms are out of scope for 0.1:

| Form | Status |
|---|---|
| `bioc::...` | Planned for 0.2, using dated Bioconductor releases |
| `local::...` | Planned for 0.2, for unpublished local packages |
| `owner/repo#123` | Pull request refs are planned for 0.2 |
| `svn::...` | Not planned; use `git::` or `url::` |

Errors for these forms must name the planned milestone or the supported
alternative shown above.

Within `[packages]`, a plain version or `"*"` means CRAN. A value containing
`/` or `::` is a remote specification. R package versions cannot contain `/`,
so the distinction is unambiguous.

## 8. Resolve first, then fetch exact source

Resolution freezes names into exact versions and commits. It performs lookups;
it does not solve dependency constraints.

### Resolve CRAN packages

Fetch `{repo}/{snapshot}/src/contrib/PACKAGES.gz` at most once per sync and
cache it. Parse its DCF stanzas and find each declared package. `"*"` selects
the version in the snapshot. An explicit version must either appear in the
snapshot or be available from the CRAN archive.

If the index contains the same package at multiple versions, select the
greatest version using R's numeric package-version ordering. The source URL is:

```text
{repo}/{snapshot}/src/contrib/{name}_{version}.tar.gz
```

### Resolve Git refs

A full 40-character commit SHA is already resolved. For a named ref or default
branch, prefer:

```text
git ls-remote <url> [<ref>]
```

This works across hosts and protocols and reuses the user's SSH keys and
credential helpers. If Git is unavailable or `ls-remote` fails for a GitHub,
GitLab, or Bitbucket specification, use that forge's public commit API as a
fallback. Branch and default-branch refs produce a warning because they can
move, but the lock always records the resolved SHA.

`@*release` needs GitHub release metadata. Try these sources in order:

1. authenticated `gh api`, honoring `GH_HOST` for GitHub Enterprise;
2. the GitHub REST API with `GITHUB_TOKEN`; and
3. the anonymous GitHub REST API.

If anonymous access is rate-limited, the error suggests installing `gh` and
running `gh auth login`.

### Fetch the resolved source

Use the first applicable method that succeeds:

| Source | Method |
|---|---|
| CRAN or `url::` | Download the HTTPS tarball into the cache |
| Public repository on github.com, gitlab.com, bitbucket.org, or codeberg.org | Download the forge archive at the resolved SHA into the cache |
| GitHub repository whose forge archive is unavailable | Request `repos/{owner}/{repo}/tarball/{sha}` through authenticated GitHub access |
| Other Git source, or a Git-backed source whose archive fetch fails | Clone the locked ref with `--depth 1` and `core.autocrlf=false`, require `HEAD` to equal the resolved SHA, and remove `.git` |

The lock records the successful method. Later reconstruction must replay that
method and artifact rather than silently substitute another one.

The `git` executable is therefore optional. It is needed when a `git::` source
requires ref resolution or cloning, for private or self-hosted hosts without
another authenticated path, and for clone fallback. It is not needed for CRAN,
`url::`, or a forge source that can be resolved through its API and downloaded
as an archive. `okr status` reports whether `git` and `gh` are available.
`okr` does not link to libgit2 or gix.

`--offline` may reuse commits already present in `okr.lock` and artifacts in the
cache. It must not download an index or archive, query an API, run
`git ls-remote`, or clone. Missing resolution data or artifacts produce a fetch
error that names what must first be prepared online.

Only declared entries are vendored in 0.1. Version 0.2 may add
`transitive = "imports"`, a traversal of `Imports` metadata that remains a
lookup rather than a solver.

## 9. Build deterministic vendor trees

Each resolved entry follows the same pipeline:

1. **Acquire.** Put downloaded artifacts in the content-addressed cache and
   verify SHA-256 before use. Reverify cache hits, and commit new artifacts with
   a temporary file followed by rename. A clone-produced source is cached after
   pruning as a normalized gzip tarball so it can also be replayed offline.
2. **Extract safely.** Extract into a temporary directory and remove the single
   archive wrapper directory. Reject absolute paths, `..` traversal, symlinks,
   and entries that are not regular files or directories.
3. **Prune by kind.** Apply case-insensitive default globs, then merge the
   entry's `exclude` globs.
4. **Replace atomically.** Write `deps-src/{name}/` through a sibling temporary
   directory and rename it into place.
5. **Hash the tree.** Inventory and hash the exact bytes left for agents.
6. **Inspect metadata.** For packages, read `Version`, `License`, and `Title`
   from `DESCRIPTION`. For references, detect a license from `LICENSE*` on a
   best-effort basis.

Package defaults exclude:

```text
data/**
pkgdown/**
docs/**
.github/**
.git*
revdep/**
**/*.rda
**/*.rds
**/*.RData
```

They also exclude `tests/**` when `include-tests = false`. The defaults retain
`R/`, `src/`, `man/`, `vignettes/`, `inst/`, `NEWS*`, `DESCRIPTION`,
`NAMESPACE`, and `LICENSE*`.

Reference defaults exclude `.git*` and nothing else. Reference repositories
are not assumed to have R package structure, so package-specific pruning does
not apply.

The tree inventory contains one record per regular file:

```text
relative/path<TAB>file-sha256
```

Paths are UTF-8, use `/` separators, and are sorted. Records are joined with LF
and have no trailing newline. Hashing this inventory with SHA-256 produces the
entry's `tree-digest`. Symlinks and other non-file entries are rejected rather
than hashed.

Clone-produced cache archives use sorted paths, zero mtimes and ownership, and
normalized modes. This makes a clean rebuild byte-identical across machines.

### Preserve the fetch method

Forge archives honor `.gitattributes` rules such as `export-ignore` and
`export-subst`; clones do not. The same commit can therefore produce different
trees through the two methods. `fetch-method` is provenance, not an incidental
optimization. The lock records the method that produced the attested tree, and
offline or bundled reconstruction reuses its cached artifact.

## 10. Separate tree integrity from library coherence

These checks answer different questions and have different defaults.

- **The vendored version must match.** A package's `DESCRIPTION` version must
  equal the version selected during resolution.
- **Tree integrity is always strict.** `verify` exits 4 whenever the vendor tree
  differs from the lock, regardless of `--strict`.
- **Installed library coherence is diagnostic by default.** During `sync` and
  `status`, report any declared package that is absent from the installed
  library or has a different version. Name both versions when available and
  print the companion installation command for the locked snapshot.
- **Strict mode attests the installed library too.** `--strict` turns drift into
  exit code 4 for `sync` and `verify`. `strict = true` under `[project]` applies
  the same rule to `sync`, `status`, and `verify`. Evaluation configurations can
  use this when the R library is part of the environment being checked.

If R is absent, library inspection is a successful skip and `status` explains
why no comparison was made.

Inspection is read-only. Locate `Rscript` and enumerate `installed.packages()`
under the project's `.libPaths()` with a short script. Never execute the
companion installation command or write to the library.

## 11. Record provenance in `okr.lock`

`okr.lock` is generated TOML. Packages and references are kept in separate
arrays and sorted by name within each array.

```toml
version = 1
okr-version = "0.1.0"
generated = "2026-06-30T00:00:00Z"
snapshot = "2026-06-30"
config-digest = "sha256:..."
environment-digest = "sha256:..."

[[package]]
name = "rpact"
version = "4.2.1"
source = "cran"
url = "https://packagemanager.posit.co/cran/2026-06-30/src/contrib/rpact_4.2.1.tar.gz"
fetch-method = "tarball"
artifact-digest = "sha256:..."
tree-digest = "sha256:..."
license = "LGPL-2.1"

[[package]]
name = "admiral"
version = "1.5.0"
source = "github::pharmaverse/admiral"
ref = "v1.5.0"
commit = "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
fetch-method = "forge-tarball"
artifact-digest = "sha256:..."
tree-digest = "sha256:..."
license = "Apache License (>= 2)"

[[reference]]
name = "cdisc-standards"
source = "git::git@ghe.corp.example:stds/cdisc.git"
ref = "2026-Q2"
commit = "77ab77ab77ab77ab77ab77ab77ab77ab77ab77ab"
fetch-method = "git-clone"
artifact-digest = "sha256:..."
tree-digest = "sha256:..."
```

`snapshot` is present only when the lock contains a CRAN package.
`generated` is deterministic: snapshot midnight when a snapshot is present,
otherwise the Unix epoch. It is never the wall-clock sync time.

The allowed fetch methods are `tarball`, `forge-tarball`, `gh`, and `git-clone`.

### Use one digest convention

Serialized attestation fields use a `<subject>-digest` key and an
`<algorithm>:<lowercase-hex>` value. The current algorithm is SHA-256.
Keeping the algorithm in the value permits a future algorithm change
without renaming every field.

The `sha256` key accepted by a direct URL declaration is different on purpose:
it is an algorithm-specific input pin, not a serialized attestation.
A Git `commit` is likewise a source-control identifier rather than
an `okr` content digest.

`config-digest` hashes the normalized configuration model.
Comments and formatting do not change it.

`environment-digest` hashes the lock schema version and the name-sorted package
and reference entries in a canonical representation.
It includes each aggregate tree digest. Evaluation harnesses record this value
for a run and ask `okr verify` to recompute it.

`artifact-digest` attests the cached acquisition artifact. "Artifact" includes
both a downloaded archive and a normalized archive created from a clone.
Every package and reference has one.

`tree-digest` attests the bytes that agents can read after extraction and
pruning. It cannot be replaced by `artifact-digest`, because the artifact and
the visible tree contain different byte sets. The per-file inventory described
in §9 is an input to this digest but is not serialized: doing so would bloat
the lockfile and manifest without adding integrity beyond the aggregate hash.
Verification therefore identifies the entry whose tree changed, not the
individual file.

## 12. Make the vendor tree discoverable

`deps-src/_manifest.md` contains separate, compact tables for packages and
references. Each row gives the name, version or commit, source, license, path,
and one-line description (`Title` from `DESCRIPTION` when available). Its
introduction says that the tree is generated, read-only reference material.

`deps-src/_manifest.json` contains the same information plus each entry's
`kind` and aggregate digests. It is schema-versioned with `"schema": 1` and is
the machine-readable interface for evaluation harnesses.

When `manifest.agents-file = true`, `okr` maintains this marker-delimited block
in `AGENTS.md`:

```markdown
<!-- okr:begin -->
Vendored R dependency sources and reference repos live in `deps-src/`
(see `deps-src/_manifest.md`). Read them to understand APIs and
internals. Do not edit them; they are generated by `okr sync` and
verified by hash.
<!-- okr:end -->
```

Only text between the markers may be replaced. If `AGENTS.md` does not exist,
`okr` creates it.

By default, `okr` also manages an entry for the vendor path in `.gitignore`.
Set `vendor.gitignore = false` when the repository should commit the source
tree. Both choices are valid because `verify` detects drift either way.

## 13. Configure the project with `okr.toml`

```toml
[project]
r-version = "4.5.1"                  # optional expected R; advisory only
snapshot = "2026-06-30"              # required when CRAN packages are declared
strict = false                       # make installed library drift fatal (§10)
# repo-url = "https://packagemanager.posit.co/cran"

[vendor]
path = "deps-src"
include-tests = true                 # tests often clarify package behavior
                                     # evaluation configs may exclude revealing tests
exclude = []                         # extra globs added to the kind defaults (§9)
gitignore = true                     # manage the vendor path in .gitignore

[manifest]
agents-file = true                   # maintain the managed block in AGENTS.md

[packages]
rpact = "*"                          # snapshot version
gsDesign = "3.6.4"                  # explicit snapshot or archive version
admiral = "pharmaverse/admiral@v1.5.0"
covr = "gitlab::jimhester/covr@abc123"
simlib = { git = "git@ghe.corp.example:stats/simlib.git", ref = "v2.1" }
internalpkg = { url = "https://example.com/internalpkg_0.2.1.tar.gz", sha256 = "..." }
rtables = { spec = "insightsengineering/rtables@v0.6.13", exclude = ["vignettes/**"] }

[references]
cdisc-standards = "git::git@ghe.corp.example:stds/cdisc.git@2026-Q2"
protocol-templates = { git = "https://codeberg.org/org/protocols.git", ref = "main" }
```

Apply these rules:

- `snapshot` is required when, and only when, `[packages]` contains a CRAN declaration.
- `r-version`, when present, is the exact `major.minor.patch` version expected
  from the project's `Rscript`. `sync` and `status` compare it with
  `R.version$major` plus `R.version$minor` and warn on a mismatch.
  `init` uses the current `Rscript` only as a convenient initial value;
  users may edit or remove it for a different target environment.
  It is never inferred from the latest R release advertised by CRAN.
  The field does not affect resolution, fetching, installation, or strict verification.
- String declarations use the grammar in §7. Table declarations add per-entry
  `spec`, `git`, `url`, `ref`, `sha256`, `exclude`, and `include-tests` options.
- A direct URL requires a SHA-256 input pin.
- Reference declarations must use Git or URL sources. A CRAN declaration under
  `[references]` is a configuration error.
- Package and reference names share the same vendor namespace and may not collide.
- Branch refs are allowed but produce a warning. The resolved SHA, not the
  branch name, provides reproducibility.
- `vendor.path` must be a safe relative path within the project.
- Unknown keys are hard errors (`deny_unknown_fields`). A misspelled option must
  never be ignored.

Configuration updates made by `okr add` must use `toml_edit`; serializing the
whole document through Serde would discard the user's formatting and comments.

## 14. Use `okr` in an evaluation

In 0.1, prepare the project and its R library while building the evaluation
image, then verify both after network access has been disabled:

```text
okr sync
# Install packages with the user's chosen tool and the snapshot shown by `okr status`.
# Copy the prepared project, R library, and any required cache into the image.

okr verify --strict --json || exit 1
```

Profiles and bundles make the preparation step portable in 0.2:

```text
okr init --profile clinical-trials   # strict=true, include-tests=false
okr sync
# Install packages with the user's chosen tool against the same snapshot.
okr bundle -o env.tar.zst --include-cache

# In the network-disabled sandbox:
okr verify --strict --json || exit 1
```

`okr` owns source context and its attestation. The harness owns the runtime,
tasks, agents, and grading.

## 15. Keep the implementation small and synchronous

### Dependencies

- `clap` with derive support
- `indicatif`
- `xshell`
- `serde`, `toml` for reads, and `toml_edit` for format-preserving writes
- `serde_json` for manifests and machine-readable output
- `reqwest` 0.13 with synchronous HTTP, `default-features = false`, and
  `features = ["blocking", "json", "rustls"]`
- `flate2` and `tar`
- `sha2`
- `globset` and `walkdir`
- `tempfile`
- `anyhow` in the binary and `thiserror` in the library
- `insta` for snapshot tests
- `assert_cmd` and `predicates` as development dependencies
- `zstd` in 0.2

Do not add an asynchronous runtime. Runtime HTTP remains synchronous.

### Modules

Keep `main.rs` thin: parse the command line, invoke the library, render the
error, and map it to the documented exit code. Put application behavior in
library modules:

- `config` for strict configuration parsing and validation;
- `spec` for the `Remotes` grammar;
- `resolve` and `resolve::dcf` for snapshot, metadata, and ref lookups; keep
  GitHub release access behind the stubbable `GithubReleaseApi` seam;
- `fetch` for synchronous acquisition and the content-addressed cache;
- `hosttools` for `git` and `gh` discovery and invocation;
- `vendor` for extraction, cloning, pruning, and atomic replacement;
- `digest` for deterministic file and tree hashes;
- `lock` for stable serialization and verification;
- `manifest` for agent-facing outputs;
- `rlib` for read-only R inspection; and
- `cli` for command orchestration.

Invoke `git` and `gh` through `xshell` with explicit argument vectors, never
through a shell. Inherit the user's environment so existing authentication
continues to work. Do not add libgit2, gix, or credential storage.

Implement the small DCF subset needed for `PACKAGES` and `DESCRIPTION` in the
crate. It must support `key: value` fields, continuation lines, and multiple
stanzas without adding another parsing dependency.

### Tests

Tests must not require a live network. Use synthetic package tarballs, a canned
`PACKAGES` index, and temporary repositories created with `git init` and
accessed through `file://` URLs. Skip a Git test only when it genuinely needs
the optional host executable.

Keep parser tests table-driven and include hostile inputs. Cover lock and
manifest formats with reviewed `insta` snapshots. Simulate R with a fake,
read-only `Rscript` on `PATH` and test warning and strict behavior separately.

Machine output is line-stable and schema-versioned. Human-readable output may
evolve without a schema change.

## 16. Milestones

### 0.1: Readable and verifiable source context

- `init`, `add`, `sync`, `status`, and `verify`
- CRAN, GitHub, GitLab, Bitbucket, `git::`, `url::`, and `@*release`
- packages and references
- public-forge archive acquisition with authenticated and clone fallbacks
- deterministic lockfile and manifests
- content-addressed cache and offline replay
- installed library diagnostics and strict mode
- release to crates.io

### 0.2: Portable evaluations and more source types

- deterministic bundles
- embedded profiles and `--profile <path-or-url>`
- `bioc::` and `local::`
- pull request refs (`owner/repo#123`)
- transitive `Imports` traversal
- a surfaced license-inventory report

System requirement discovery and Dockerfile generation are intentionally absent
from the roadmap; the boundary in §4 continues to apply.

## 17. Resolve the remaining design questions

1. **What should happen when only some entries can be fetched?**
   The proposed default is to fail with exit code 3. A partial tree is easy to
   mistake for a complete one. A future `--keep-going` option could support
   exploratory diagnosis.
2. **Should transitive vendoring include `Suggests`?**
   The proposed answer is no. Keep optional sources explicit and make
   `okr add` the inexpensive way to opt in.
3. **Where should profiles live?**
   Start 0.2 with one or two profiles embedded in the binary.
   Move to a separately maintained `okr-profiles` repository
   only when its maintenance and review model are clear.
