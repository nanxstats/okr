# okr design spec

> okr is not okr. It's okay-R: a context and provenance layer that makes R
> projects legible to AI coding agents and reproducible for offline evaluation.

## 1. Problem statement

An installed R library is opaque to AI coding agents. R source in installed
packages is stored in binary lazy-load databases (`.rdb`/`.rdx`), and compiled
`src/` code is stripped at install time. An agent pointed at `.libPaths()` can
read `DESCRIPTION` and `NAMESPACE` and essentially nothing else. This makes R
uniquely hostile to grep/glob-driven agent workflows, in contrast to Python
(plain `.py` in site-packages) and Rust (`cargo vendor`).

Separately, AI model evaluation on R coding tasks requires hermetic, offline,
verifiable environments: pinned source trees and a digest that eval harnesses
can assert against, so that models cannot cheat via network access and so
that runs are reproducible and auditable.

`okr` solves both with one mechanism: a **vendored source tree** of a
project's R dependencies and reference repositories (a "synthetic monorepo"),
driven by a TOML config, recorded in a lockfile, and verifiable by hash.

## 2. Positioning

`okr` retrieves, organizes, and attests source context. It **never installs
anything**: not R, not packages, and never mutates the user's toolchain or
library. Installation is a solved problem owned by dedicated tools; okr is
designed to sit beside them:

| Concern | Owner |
|---|---|
| R toolchain installation | `rig` |
| Package installation & dependency resolution | `pak` / `renv` / `rv` / `install.packages()` |
| Version pinning across time | Posit Package Manager dated snapshots |
| Source legibility, provenance, verification | **okr** |

okr reads the installed library (when R is present) purely as a diagnostic,
and its docs show the one-line companion install command against the same
snapshot, but executing it is the user's (or their package manager's) job.

## 3. Goals

1. Vendor exact-version source trees of declared R packages and arbitrary
   reference git repositories into the project.
2. Accept the de facto standard R `Remotes` syntax for declaring sources.
3. Guarantee **version--source coherence** between the vendored tree and the
   declared/locked versions; *diagnose* (not enforce, by default) coherence
   with the user's installed library.
4. Produce a lockfile with content hashes sufficient for byte-level
   verification and an environment digest for stamping benchmark runs.
5. Generate agent-facing manifests so coding agents discover the vendored
   tree without prompting.
6. Support fully offline operation from a local cache or bundle.
7. Work against any git host: public forges, GitHub Enterprise, self-hosted
   GitLab, Codeberg, bare `git://` remotes, SSH remotes, using the user's
   existing git/`gh` authentication rather than managing credentials itself.

## 4. Non-goals

- **No installation of anything.** No R setup, no package installation,
  no library mutation. okr emits commands and diagnostics only.
- **No dependency version solver.** CRAN resolution is a metadata lookup
  against the snapshot's PACKAGES index.
- **No eval runner.** No task definitions, grading, or agent loops. okr emits
  machine-readable manifests for harnesses (for example, Inspect) to consume.
- **No credential management.** GitHub auth is delegated to `gh` or
  `GITHUB_TOKEN`; git auth is delegated to the user's git configuration.
- **No multi-language support.** R's opacity is the reason okr exists.
- **No renv/rv interop or lockfile conversion.**

## 5. Core concepts

- **`okr.toml`** - declarative project config (committed).
- **`okr.lock`** - resolved versions, refs, URLs, hashes, digests (committed).
- **Vendor tree** - `deps-src/` by default; one directory per entry.
- **Package** - an R package, vendored at an exact version; participates in
  version resolution and library-coherence diagnostics.
- **Reference** - an arbitrary git repository vendored for agent context (a
  standards repo, a protocol-template repo, a non-R codebase). No version
  semantics beyond the pinned commit; no coherence involvement; minimal
  pruning.
- **Snapshot** - a dated PPM repository
  (`https://packagemanager.posit.co/cran/<YYYY-MM-DD>`) used to resolve and
  fetch CRAN sources. Required iff any CRAN package is declared.
- **Cache** - content-addressed download cache at `$OKR_CACHE_DIR`
  (default `~/.cache/okr`), keyed by sha256.
- **Manifest** - `deps-src/_manifest.json` (machine) and
  `deps-src/_manifest.md` (agent/human). R package names must start with a
  letter, so `_`-prefixed files cannot collide with package directories.
- **Profile** - a curated, shippable `okr.toml` template (for example, `tdb`,
  short for "trial design bench"), selected at `okr init`.
  Breadth lives in profiles; the tool stays narrow.
- **Bundle** - a deterministic archive of config + lock + vendor tree +
  cached artifacts for air-gapped environments.

## 6. CLI surface

```
okr init [--profile <name>] [--force]
okr add <spec>... [--reference]
okr sync [--offline] [--strict]
okr status [--json]
okr verify [--json] [--strict]
okr bundle [-o <path>] [--include-cache]      # milestone 0.2
okr sysreqs [--os ubuntu-26.04]               # milestone 0.3 (prints info only)

Global flags: --config <path>  --quiet  --verbose  --json (where noted)
```

### Command semantics

**`init`** - Write `okr.toml` (from a profile template if given). Refuses to
overwrite without `--force`. Offers to append the vendor path to `.gitignore`
(see §12).

**`add`** - Parse each `<spec>` per the grammar in §7 and insert it into
`[packages]` (or `[references]` with `--reference`), preserving user comments
and formatting (`toml_edit`). Does not run sync.

**`sync`** - The convergence command; idempotent. Pipeline:

1. *Resolve.* Determine the exact version/commit and fetch plan for every
   declared entry (§8).
2. *Fetch & vendor.* Download or clone (or hit cache), verify, extract,
   prune, write trees (§9).
3. *Lock.* Write `okr.lock` (§11).
4. *Manifest.* Write `_manifest.json` / `_manifest.md`; update the marker
   block in `AGENTS.md` if enabled (§12).
5. *Coherence diagnostic.* If R is present, compare installed package
   versions against vendored versions; **warn** on mismatch, listing the
   companion install command that would reconcile. With `--strict` (or
   `project.strict = true`): exit 4 instead (§10).

Re-running `sync` with an up-to-date lock and intact tree is a no-op
(fast-path via digest comparison).

**`status`** - Report: R presence/version (advisory vs `project.r-version`),
lock freshness (config hash vs lock), vendor-tree integrity spot-check,
library coherence summary, cache stats, and the copy-paste companion install
line, for example:

```
install with:  Rscript -e 'pak::pkg_install(c("rpact","gsDesign"), repos="https://packagemanager.posit.co/cran/2026-06-30")'
```

**`verify`** - Re-hash the full vendor tree against `okr.lock`. Tree
integrity is **always** a hard check: exit 0 clean, exit 4 on any drift;
`--json` lists per-file mismatches. `--strict` additionally runs the library
coherence check as hard (for eval sandboxes where the installed library is
part of the attested environment). This is the eval harness's integrity gate.

**`bundle`** (0.2) - Deterministic `tar.zst` containing `okr.toml`,
`okr.lock`, `deps-src/`, and (with `--include-cache`) the artifacts needed to
rebuild the tree. Determinism rules: entries sorted by path, mtimes zeroed,
uid/gid zeroed, modes normalized (0644/0755). Bundle sha256 is printed and
reproducible across machines.

**`sysreqs`** (0.3) - Query PPM system-requirements metadata for declared
packages and print OS package lists (apt/dnf). Informational only; okr never
installs them.

### Exit codes

`0` success · `1` unexpected error · `2` config/spec error · `3`
network/fetch error · `4` verification or strict-coherence failure.

## 7. Source spec grammar (R `Remotes` subset)

okr adopts the syntax R developers already know from the `Remotes:` field
(remotes/devtools/pak), as both the `okr add` argument format and the string
form in `okr.toml`:

```
spec        := [type "::"] body ["@" ref]
type        := "github" | "gitlab" | "bitbucket" | "git" | "url"
body        := owner "/" repo            (forge types; github is the default type)
             | <git URL>                 (git:: - any host, any protocol git supports)
             | <http(s) URL to tarball>  (url::)
ref         := tag | branch | commit SHA | "*release"   (*release: GitHub only)
```

| Spec | Meaning |
|---|---|
| `pharmaverse/admiral@v1.3.0` | GitHub (default type), tag |
| `github::tidyverse/ggplot2` | GitHub, default branch (warns; SHA is locked) |
| `r-lib/testthat@*release` | GitHub, latest release (resolved then frozen) |
| `gitlab::jimhester/covr@abc123` | gitlab.com, commit |
| `bitbucket::sulab/mygene.r@default` | bitbucket.org, branch |
| `git::git@ghe.corp.example:stats/simlib.git@v2.1` | any git host via user's git auth |
| `git::https://codeberg.org/org/pkg.git@v1.0` | ditto |
| `url::https://example.com/pkg_0.2.1.tar.gz` | direct tarball (`sha256` required in table form) |

Status of the remaining `Remotes` types - errors are instructive and name the
milestone or alternative:

| Spec form | Status |
|---|---|
| `bioc::...` | **Planned (0.2)** - Bioconductor dated releases map cleanly onto okr's model |
| `local::...` | **Planned (0.2)** - local path vendoring for unpublished internal packages |
| `owner/repo#123` (PR refs) | **Planned (0.3)** |
| `svn::...` | **Rejected permanently** - use `git::` or `url::` |

In `[packages]`, a plain version string or `"*"` means CRAN-from-snapshot;
any string containing `/` or `::` is parsed as a remote spec (R package
versions never contain `/`, so this is unambiguous).

## 8. Resolution & fetch strategy

Resolution is a lookup, never a solver:

- **CRAN/snapshot:** fetch `{repo}/{snapshot}/src/contrib/PACKAGES.gz` once
  per sync (cached), parse DCF, find each declared package's version (or
  validate an explicit pin, falling back to the CRAN archive URL for
  superseded versions). Tarball: `{...}/src/contrib/{name}_{version}.tar.gz`.
- **Git refs &rarr; commit SHA:** `git ls-remote <url> <ref>` - universal across
  hosts and protocols, uses the user's existing SSH keys and credential
  helpers, has no API rate limits. A spec pinned to a full SHA skips
  resolution. `@*release` (GitHub only) resolves via the tier below.
- **GitHub API needs** (`@*release`, private tarball download), in order:
  1. `gh` CLI, if on PATH and authenticated (`gh api ...`; honors `GH_HOST`
     for GitHub Enterprise).
  2. Direct REST with `GITHUB_TOKEN` from the environment.
  3. Anonymous REST (low rate limits; a 403 produces a hint to install `gh`
     and run `gh auth login`).

**Fetch (download of the resolved commit), fastest applicable path wins:**

| Source situation | Method |
|---|---|
| CRAN / `url::` | HTTPS tarball &rarr; cache |
| Public repo on a recognized forge (github.com, gitlab.com, bitbucket.org, codeberg.org) | Forge archive tarball at the resolved SHA &rarr; cache |
| Private GitHub | `gh api repos/{o}/{r}/tarball/{sha}` when `gh` available |
| Anything else (`git::`, GHE, self-hosted, tarball fetch failed) | Shallow `git clone` at the ref (`--depth 1`, `core.autocrlf=false`), verify `HEAD` == resolved SHA, strip `.git` |

The `git` binary is therefore an **optional runtime dependency**: required
only for `git::` sources and private/self-hosted hosts; never required for
CRAN, `url::`, or public-forge sources. `okr status` reports git/`gh`
availability. okr never links libgit2/gix and never manages credentials.

Transitive vendoring is **out of scope for 0.1** (declared entries only).
0.2 may add `transitive = "imports"` as a PACKAGES-metadata traversal:
still not a version solver.

## 9. Vendoring pipeline

1. Acquire per §8; tarballs land in the cache named by sha256 and are
   verified before use. `--offline` requires a cache hit (else exit 3,
   naming the missing artifact); clone-fetched sources are cached as a
   normalized tarball of the pruned tree so offline mode covers them too.
2. Extract to a temp dir; strip the top-level directory.
3. Prune, by kind (globs, case-insensitive):
   - **Packages:** exclude `data/**`, `pkgdown/**`, `docs/**`, `.github/**`,
     `.git*`, `revdep/**`, `**/*.rda`, `**/*.rds`, `**/*.RData`, plus
     `tests/**` when `include-tests = false`. Kept by design: `R/`, `src/`,
     `man/`, `vignettes/`, `inst/`, `NEWS*`, `DESCRIPTION`, `NAMESPACE`,
     `LICENSE*`.
   - **References:** exclude only VCS metadata (`.git*`) by default:
     reference repos are not R packages and R-motivated pruning must not apply.
   - User `exclude` globs merge on top in both cases.
4. Write to `deps-src/{name}/`, replacing atomically (sibling temp dir,
   rename).
5. Compute the **tree digest**: sha256 over the sorted list of
   `(relative-path, file-sha256)` pairs, newline-joined, `/`-normalized
   paths. Byte-stable across platforms.
6. Packages: parse `DESCRIPTION` for `Version` and `License`. References:
   best-effort license detection from `LICENSE*`; no version beyond the SHA.

**Fetch-method determinism caveat:** forge archive tarballs honor
`.gitattributes` `export-ignore`/`export-subst`, while clones do not: the
same commit can yield different trees by method. The lockfile therefore
records `fetch-method`, the digest attests the tree as actually produced,
and offline/bundle reproduction replays the cached artifact rather than
re-deriving by a different method.

## 10. Coherence & strictness

okr cannot fix an installed library it refuses to touch, so by default it
diagnoses instead of blocking (pit of success):

- **Default:** during `sync` and `status`, if R is present and an installed
  package's version differs from the vendored version, emit a prominent
  warning naming both versions and the companion install command against the
  locked snapshot. If R is absent, the check is skipped and `status` says so.
- **Strict:** `--strict` on `sync`/`verify`, or `strict = true` under
  `[project]`, upgrades mismatches to exit 4. Eval profiles set this:
  in a benchmark sandbox the installed library is part of the attested
  environment and drift must be fatal.
- **Never relaxed:** `verify`'s vendor-tree integrity check (tamper
  detection) is hard regardless of strictness.

Library introspection is read-only: locate R, run a short `Rscript` snippet
to enumerate `installed.packages()` fields (or parse DESCRIPTION files under
`.libPaths()` directly), and never write.

## 11. Lockfile schema: `okr.lock`

TOML, generated, stable ordering (entries sorted by name within kind):

```toml
version = 1                          # lockfile schema
okr-version = "0.1.0"
generated = "2026-08-11T17:03:00Z"
snapshot = "2026-06-30"              # present iff CRAN entries exist
config-hash = "sha256:..."             # normalized okr.toml hash, staleness detection
environment-digest = "sha256:..."      # hash over all entries below; the benchmark stamp

[[package]]
name = "rpact"
version = "4.2.1"
source = "cran"
url = "https://packagemanager.posit.co/cran/2026-06-30/src/contrib/rpact_4.2.1.tar.gz"
fetch-method = "tarball"
tarball-sha256 = "..."
tree-digest = "sha256:..."
license = "LGPL-2.1"

[[package]]
name = "admiral"
version = "1.3.0"
source = "github::pharmaverse/admiral"
ref = "v1.3.0"
commit = "9f2c..."
fetch-method = "forge-tarball"       # tarball | forge-tarball | gh | git-clone
tarball-sha256 = "..."
tree-digest = "sha256:..."
license = "Apache License (>= 2)"

[[reference]]
name = "cdisc-standards"
source = "git::git@ghe.corp.example:stds/cdisc.git"
ref = "2026-Q2"
commit = "77ab..."
fetch-method = "git-clone"
tree-digest = "sha256:..."
```

`environment-digest` is deterministic given identical lock content and is
the value an eval harness records per run and asserts via `okr verify`.

## 12. Agent affordances

- `deps-src/_manifest.md` - compact tables, packages and references listed
  separately: name, version (or commit), source, license, path, one-line
  description (`Title` from DESCRIPTION where available). Header text tells
  an agent what this tree is and that it is read-only reference material.
- `deps-src/_manifest.json` - the same, machine-readable, plus digests and
  `kind` per entry; schema versioned (`"schema": 1`). This is the
  integration surface for eval harnesses.
- `AGENTS.md` marker block (when `manifest.agents-file = true`):

  ```
  <!-- okr:begin -->
  Vendored R dependency sources and reference repos live in `deps-src/`
  (see `deps-src/_manifest.md`). Read them to understand APIs and
  internals. Do not edit them; they are generated by `okr sync` and
  verified by hash.
  <!-- okr:end -->
  ```

  Only the marker block is ever touched; the file is created if absent.
- `.gitignore`: default-managed entry for the vendor path. Committing the
  tree is a legitimate choice for benchmark repos (`gitignore = false`)
  because `verify` makes drift detectable either way.

## 13. Configuration schema: `okr.toml`

```toml
[project]
name = "trial-design-bench"          # optional
r-version = "4.5.1"                  # optional, advisory (status/diagnostics only)
snapshot = "2026-06-30"              # required iff any CRAN package is declared
strict = false                       # coherence mismatches fatal when true (§10)
# repo-url = "https://packagemanager.posit.co/cran"   # mirror override

[vendor]
path = "deps-src"
include-tests = true                 # dependency test suites are high-signal for agents...
                                     # ...but may leak answers into benchmark sandboxes; eval profiles set false
exclude = []                         # extra globs, merged with kind defaults (§9)
gitignore = true                     # manage a .gitignore entry for the vendor path

[manifest]
agents-file = true                   # maintain a marker-delimited block in AGENTS.md

[packages]                           # name = version | "*" | remote spec (§7) | table
rpact = "*"                          # CRAN, version from snapshot
gsDesign = "3.6.4"                   # CRAN, explicit pin (snapshot or archive)
admiral = "pharmaverse/admiral@v1.3.0"
covr = "gitlab::jimhester/covr@abc123"
simlib = { git = "git@ghe.corp.example:stats/simlib.git", ref = "v2.1" }
internalpkg = { url = "https://example.com/internalpkg_0.2.1.tar.gz", sha256 = "..." }
rtables = { spec = "insightsengineering/rtables@v0.6.13", exclude = ["vignettes/**"] }

[references]                         # arbitrary git repos for agent context; no version semantics
cdisc-standards = "git::git@ghe.corp.example:stds/cdisc.git@2026-Q2"
protocol-templates = { git = "https://codeberg.org/org/protocols.git", ref = "main" }
```

Rules:

- String values are parsed per §7; tables allow per-entry options
  (`spec`/`git`/`url`, `ref`, `sha256`, `exclude`, `include-tests`).
- `[references]` entries must be git or url sources: CRAN specs there are a
  config error.
- Branch refs are accepted but warn; the resolved SHA is what gets locked,
  so reproducibility is preserved either way.
- Unknown keys are a hard config error (deny_unknown_fields): typos must not
  silently no-op.

## 14. Benchmark / eval usage pattern

```
okr init --profile clinical-trials       # profile sets strict=true, include-tests=false
okr sync                                 # on the image-build machine, online
# install the library with your tool of choice against the same snapshot (see `okr status`)
okr bundle -o env.tar.zst --include-cache
# inside the sandbox (network off):
okr verify --strict --json || exit 1     # integrity gate before the agent runs
```

okr provides the environment's context layer and its attestation; the
harness owns tasks, agents, and grading.

## 15. Implementation notes

- **Crates:**
  - `clap` (derive)
  - `indicatif`
  - `xshell`
  - `serde` + `toml` (read) + `toml_edit` (format-preserving writes)
  - `reqwest` 0.13 (sync HTTP; no async runtime in 0.1, default-features = false, features = ["blocking", "json", "rustls",])
  - `flate2` + `tar`
  - `sha2`
  - `globset` + `walkdir`
  - `tempfile`
  - `anyhow` (bin) + `thiserror` (lib)
  - `insta` (snapshot tests)
  - `zstd` (0.2)
  - `assert_cmd` (dev-dependencies)
  - `predicates` (dev-dependencies)
  - `proptest` (dev-dependencies)
- **Structure:** thin `main.rs`; library crate with modules `config`, `spec`
  (Remotes-grammar parser), `resolve`, `fetch` (tiered strategy + cache),
  `vendor`, `digest`, `lock`, `manifest`, `rlib` (read-only R library
  introspection), `hosttools` (git/`gh` detection & shell-out), `cli`.
- **Shell-outs:** `git` and `gh` are invoked using `xshell` crate with explicit
  argument vectors (never through a shell), environment passed through so
  user auth works. No libgit2/gix.
- **DCF parsing** (PACKAGES, DESCRIPTION): implement minimally in-crate
  (continuation lines, `key: value`); ~50 lines, avoids a dependency.
- **No live network in tests.** Fixtures: synthetic package tarballs, a
  canned PACKAGES file, and local `git init`-created repositories exercised
  via `file://` URLs (covers ls-remote and clone paths without network).
- Machine output (`--json`) is line-stable and schema-versioned; human
  output may change freely.

## 16. Milestones

- **0.1 - the wedge:** `init`, `add`, `sync`, `status`, `verify`; spec
  grammar per §7 (CRAN, github/gitlab/bitbucket, `git::`, `url::`,
  `@*release`); `[references]`; tiered fetch with `gh`/token/anonymous and
  git-clone fallback; lockfile; manifests; cache; offline mode; coherence
  diagnostics with `--strict`. Publishable to crates.io.
- **0.2 - eval hardening & reach:** `bundle` (deterministic archives),
  profiles (embedded + `--profile <path-or-url>`), `bioc::` and `local::`
  sources, transitive vendoring (`"imports"` traversal), license inventory
  surfaced as a report.
- **0.3 - operations:** `sysreqs` (informational), Dockerfile emission
  (`okr bundle --docker`), PR refs (`owner/repo#123`).

## 17. Open questions

1. Should `sync` fail (exit 3) or degrade when *some* entries fetch and
   others don't? Lean: fail by default (a partial context tree silently
   missing a dependency is worse than no update), with `--keep-going` for
   exploratory use.
2. Vendor `Suggests`-adjacent sources for declared packages? Lean: no.
   Explicitness wins; `add` makes opting in cheap.
3. Profile distribution: embedded in the binary vs. a community
   `okr-profiles` repo fetched by name. Lean: embed 1 to 2 reference profiles
   in 0.2, move to a repo when a second maintainer shows up.
