---
icon: lucide/boxes
---

# Context model

`okr` treats source context as a separate project artifact, alongside the
installed R library rather than inside it.

## The opacity problem

After an R package is installed, much of its implementation is no longer
available as ordinary source files. R functions are stored in `.rdb` and `.rdx`
lazy-load databases, while compiled source under `src/` is omitted. A coding
agent that relies on file search can see `DESCRIPTION` and `NAMESPACE`, but not
the complete implementation it needs to reason about APIs and internals.

`okr` retrieves package source archives and Git repositories before that
information is lost. It places their useful contents under one project-relative
directory, creating a synthetic monorepo that ordinary search tools and coding
agents can inspect.

## Project artifacts

| Artifact | Role | Maintained by |
|---|---|---|
| `okr.toml` | Human-reviewed source declarations and policy | You, optionally through `okr add` |
| `okr.lock` | Exact resolved provenance and integrity digests | `okr sync` |
| `deps-src/<name>/` | Pruned source trees agents actually read | `okr sync` |
| `deps-src/_manifest.md` | Compact human and agent index | `okr sync` |
| `deps-src/_manifest.json` | Schema-versioned integration surface | `okr sync` |
| Optional `AGENTS.md` marker | Persistent discovery instructions when explicitly enabled | `okr sync` |

The configured vendor path can replace `deps-src`, but it must remain a safe
relative path below the project.

## Convergence workflow

`okr sync` turns declarations into an attested source tree:

1. Parse and validate the strict configuration.
2. Resolve CRAN versions and remote refs to exact sources.
3. Acquire verified archives or clone through the user's host tools.
4. Extract safely, prune by entry kind, and atomically replace the vendor tree.
5. Hash the resulting files, write the lock and manifests, then inspect the R
   library read-only when `Rscript` is available.

Repeating this workflow with an unchanged configuration and intact tree takes a
verification fast path. Rebuilding from the same locked artifacts produces the
same normalized metadata and tree digests.

## Two entry kinds

Packages and references share acquisition and attestation machinery but serve
different purposes.

An R **package** has an exact package version read from `DESCRIPTION`. It is
included in installed-library coherence checks and is pruned with R-specific
rules.

A **reference** is any repository useful to an agent: a standard, protocol,
template, example corpus, or non-R codebase. It has no package-version semantics
and keeps nearly its entire tree. Its resolved commit or verified archive is
still locked and hashed.

This distinction lets a project provide broad context without pretending every
source is an installable R dependency.
