---
icon: lucide/package-open
---

# Quick start

Run these commands from the root of an existing R project.

## Initialize the project

```console
okr init
```

`init` searches backward from the current UTC date for the latest available
dated Posit (Public) Package Manager CRAN snapshot. It writes that exact date to
`okr.toml` and caches the successful package index for the first sync. It also
adds the default `deps-src/` path to `.gitignore`. When `.Rbuildignore` already
exists, `init` adds anchored rules for the vendor path, `okr.toml`, and
`okr.lock` so they stay out of R package tarballs. If `Rscript` is available,
it records the exact detected R version as an advisory `project.r-version`;
otherwise the optional field is omitted.

`init` refuses to replace an existing configuration unless `--force` is given.

## Declare the context

Add a CRAN package and a package hosted on GitHub:

```console
okr add ggsci
okr add pharmaverse/admiral@v1.5.0
```

Bare names become `"*"` entries under `[packages]`, resolved from the configured
snapshot. Remote specifications use the familiar R `Remotes` style. `add`
updates `okr.toml` without running a sync and preserves its comments and
formatting.

To provide a non-package repository as background material for the agent, use a
reference declaration:

```console
okr add --reference git::https://codeberg.org/org/protocols.git@main
```

Only entries you declare are vendored; milestone 0.1 does not traverse package
dependencies.

## Build the source context

```console
okr sync
```

`sync` resolves exact versions and commits, acquires verified artifacts, prunes
and writes the trees, then creates the lockfile and manifests. A typical result
looks like this:

```text
okr.toml
okr.lock
deps-src/
├── _manifest.json
├── _manifest.md
├── admiral/
├── ggsci/
└── protocols/
```

When asking a coding agent to use the vendored context, point it to
`deps-src/_manifest.md` and tell it to treat the source tree as read-only. To
persist that pointer in `AGENTS.md`, explicitly set
`manifest.agents-file = true`; it is disabled by default. Re-running `okr sync`
with unchanged configuration and an intact tree takes the verification fast
path and reports that the project is already synchronized.

## Inspect and verify

```console
okr status
okr verify
```

`status` reports lock freshness, vendor integrity, host-tool availability,
cache usage, and installed-library coherence when R is available. It also
prints a companion package-install command against the locked snapshot, but
never runs it.

`verify` hashes the complete vendored tree and generated manifests. Any drift
is a hard failure with exit code 4. Add `--strict` when the installed R library
is also part of the environment you want to attest:

```console
okr verify --strict --json
```

The JSON form is schema-versioned for use by CI and evaluation harnesses.
