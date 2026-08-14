# okr

[![crates.io](https://img.shields.io/crates/v/okr.svg)](https://crates.io/crates/okr)
[![CI tests](https://github.com/nanxstats/okr/actions/workflows/ci.yml/badge.svg)](https://github.com/nanxstats/okr/actions/workflows/ci.yml)

> Reproducible R source context for coding agents.

Installed R packages are poor context for coding agents. R code is packed into
binary lazy load databases (`.rdb`/`.rdx`), while compiled package `src/` trees
are stripped at installation time. `okr` vendors exact source trees into a
greppable synthetic monorepo with records hashes.

## Install

```console
cargo install okr
```

## Quickstart

```console
okr init
okr add ggsci
okr sync
# Point your coding agent at deps-src/
```

`okr init` selects and writes the latest available exact dated CRAN snapshot,
so a bare CRAN package can be added immediately. It checks from the current UTC
date backward and keeps the successful package index in the download cache.

`okr sync` writes `okr.lock`, `deps-src/_manifest.json`, and
`deps-src/_manifest.md`. Repeating it with an intact tree is a no-op.

## Evaluation workflow

Build the source context online, then use the integrity gate in an evaluation
sandbox:

```console
okr sync
okr verify --strict --json
```

`verify` always treats vendor-tree drift as an exit-4 failure. `--strict` also
requires the read-only installed-library coherence check when R is available.
Use `okr sync --offline` to rebuild entirely from the local content-addressed
cache. The compact lock records an acquisition-artifact hash and one aggregate
digest for each post-pruning source tree; verification recomputes the latter
from every vendored file.

## GitHub authentication

`okr` uses the `gh` CLI when present and authenticated; otherwise it uses
`GITHUB_TOKEN`; otherwise it uses anonymous GitHub API access with low rate
limits. Private GitHub repositories use the same tiers before falling back to
git. GitHub Enterprise and self-hosted GitLab, Codeberg, SSH, and other
`git::` remotes use your existing git credential helpers and SSH configuration;
`okr` never stores credentials.

## Scope

`okr` installs nothing: rig, pak, renv, and rv do that.

## License

MIT
