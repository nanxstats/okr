# okr

[![crates.io](https://img.shields.io/crates/v/okr.svg)](https://crates.io/crates/okr)
[![CI tests](https://github.com/nanxstats/okr/actions/workflows/ci.yml/badge.svg)](https://github.com/nanxstats/okr/actions/workflows/ci.yml)
[![Documentation](https://github.com/nanxstats/okr/actions/workflows/docs.yml/badge.svg)](https://nanx.me/okr/)

> okr is not okr. It's okay-R: a context and provenance layer that makes R
> projects legible to AI coding agents and reproducible for offline evaluation.

Pronounce **okr** "okay-R." The name is a pun: this is an R source context and
reproducibility tool, not an "objectives and key results" tool.

Installed R libraries are poor context for coding agents. R code is packed into
binary lazy load databases (`.rdb`/`.rdx`), while compiled package `src/` trees
are stripped at installation time. `okr` vendors exact source trees into a
greppable synthetic monorepo and records hashes that can be checked offline.

## Install

```console
cargo install okr
```

## Quickstart

```console
okr init
okr add pharmaverse/admiral@v1.3.0
okr sync
# Point your coding agent at deps-src/
```

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
cache.

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
