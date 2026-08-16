---
icon: lucide/house
---

# okr

[![crates.io](https://img.shields.io/crates/v/okr.svg)](https://crates.io/crates/okr)
[![CI tests](https://github.com/nanxstats/okr/actions/workflows/ci.yml/badge.svg)](https://github.com/nanxstats/okr/actions/workflows/ci.yml)
[![Documentation](https://github.com/nanxstats/okr/actions/workflows/docs.yml/badge.svg)](https://nanx.me/okr/)

> Reproducible R source context for coding agents.

`okr` retrieves exact R package sources and arbitrary reference repositories,
organizes them into a greppable source tree, and records enough provenance to
verify every byte later.

## Why

An installed R library is poor context for a coding agent. Package R code is
stored in binary lazy-load databases, and compiled `src/` trees are removed
during installation. Metadata remains visible, but much of the implementation
the agent needs to inspect does not.

`okr` creates a synthetic monorepo alongside the project. It can contain exact
CRAN package versions, packages from Git forges, and non-package repositories
that provide useful standards, protocols, or examples. A lockfile and aggregate
tree digests make the result suitable for reproducible, offline evaluation as
well as day-to-day agent work.

## Start here

```console
cargo install okr
okr init
okr add ggsci
okr sync
```

The result includes:

- `okr.toml`, the declarations you maintain;
- `okr.lock`, the resolved versions, commits, acquisition methods, and digests;
- `deps-src/`, the pruned source trees;
- `deps-src/_manifest.md` and `_manifest.json`, indexes for people, agents, and
  evaluation harnesses; and
- a managed marker block in `AGENTS.md` that tells coding agents where to look.

Continue with the [quick start](guide/quickstart.md), then see
[source declarations](guide/sources.md) for CRAN, GitHub, GitLab, Bitbucket,
arbitrary Git hosts, direct tarballs, and reference repositories.

## Less is more

`okr` installs nothing. It does not install R, mutate an R library, or solve
package dependencies. Use `rig` for R toolchains and `pak`, `renv`, `rv`, or
`install.packages()` for package installation. `okr` can inspect the installed
library read-only and report whether its versions agree with the vendored
sources. System requirement discovery and container construction also remain
with those environment tools: use `renv::sysreqs()` or `pak::pkg_sysreqs()`,
then follow renv's Docker workflow when building an image.
okr's lock, vendored tree, manifests, and verification command compose into
that workflow without owning it.
