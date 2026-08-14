---
icon: lucide/git-fork
---

# Source declarations

`okr` supports CRAN snapshot entries and the milestone 0.1 subset of the R
`Remotes` syntax. Package dependencies are not discovered transitively: every
package or reference repository you want in the source tree must be declared.

## CRAN packages

Under `[packages]`, `"*"` selects the package version present in the configured
dated snapshot. An explicit version pins that version; if it is no longer in
the snapshot index, `okr` uses the corresponding CRAN archive URL.

```toml
[project]
snapshot = "2026-06-30"

[packages]
rpact = "*"
gsDesign = "3.6.4"
```

`okr add rpact` writes the wildcard form. Resolution is a lookup in
`PACKAGES.gz`, not a dependency or version-constraint solver.

## Remote grammar

```text
[type::]body[@ref]
```

| Type | Body | Example |
|---|---|---|
| GitHub, implicit | `owner/repo` | `pharmaverse/admiral@v1.3.0` |
| `github::` | `owner/repo` | `github::tidyverse/ggplot2@main` |
| `gitlab::` | `owner/repo` | `gitlab::jimhester/covr@abc123` |
| `bitbucket::` | `owner/repo` | `bitbucket::sulab/mygene.r@default` |
| `git::` | Any URL understood by Git | `git::git@ghe.example:stats/simlib.git@v2.1` |
| `url::` | HTTP(S) `.tar.gz` or `.tgz` URL | Use table form with `sha256`; see below. |

A ref may be a branch, tag, abbreviated commit, or full 40-character commit.
Named refs are resolved and frozen to the exact commit in `okr.lock`. Branches
and omitted refs produce a warning because their future target can move, but
the completed lock remains reproducible. A full commit skips remote ref
resolution.

GitHub alone also supports `@*release`:

```console
okr add r-lib/testthat@*release
```

The latest release tag is resolved through authenticated `gh`,
`GITHUB_TOKEN`, or anonymous GitHub API access, then frozen to its commit.

## Arbitrary Git hosts

Use `git::` for GitHub Enterprise, self-hosted GitLab, Codeberg, SSH remotes,
or any other transport supported by the host `git` executable:

```toml
[packages]
simlib = { git = "git@ghe.example:stats/simlib.git", ref = "v2.1" }

[references]
protocols = "git::https://codeberg.org/org/protocols.git@main"
```

`okr` invokes Git with explicit arguments and inherits your SSH configuration
and credential helpers. It neither prompts for nor stores credentials.

## Direct tarballs

A direct URL must use table form with the expected SHA-256 of the downloaded
archive:

```toml
[packages]
internalpkg = {
  url = "https://example.com/internalpkg_0.2.1.tar.gz",
  sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
}
```

This digest is checked before the artifact is committed to the cache. The
archive must contain one top-level directory and safe regular-file entries.
Symlinks, special files, absolute paths, and path traversal are rejected.

## Packages and references

The two sections intentionally behave differently.

| Kind | Purpose | Version semantics | Default pruning |
|---|---|---|---|
| `[packages]` | R package implementation context | Exact package version plus optional commit | R-specific size reduction |
| `[references]` | Standards, protocols, examples, or other repositories | Exact commit or verified tarball; no R package version | Version-control metadata only |

References cannot use a CRAN-shaped value such as `"*"` or `"1.2.3"`. Add a
Git reference from the CLI with `okr add --reference <spec>`.

## Pruning

Package vendoring excludes these paths by default, with case-insensitive glob
matching:

- `data/**`, `pkgdown/**`, `docs/**`, `.github/**`, `revdep/**`;
- Git metadata matched by `.git*`;
- serialized data files such as `*.rda`, `*.rds`, and `*.RData`; and
- `tests/**` when `include-tests = false`.

Source and documentation useful to agents remain, including `R/`, `src/`,
`man/`, `vignettes/`, `inst/`, `NEWS*`, `DESCRIPTION`, `NAMESPACE`, and
licenses. Reference repositories use only the version-control exclusions by
default because R-specific pruning would discard potentially important
context.

Global `vendor.exclude` patterns and per-entry `exclude` patterns are merged on
top of these defaults. Pruning affects the tree digest, so changing a pattern
requires a new `okr sync`.

## Not yet supported

| Form | Status |
|---|---|
| `bioc::...` | Planned for milestone 0.2. |
| `local::...` | Planned for milestone 0.2. |
| `owner/repo#123` | Pull-request refs are planned for milestone 0.3. |
| `svn::...` | Permanently unsupported; use `git::` or a verified `url::` tarball. |

Profiles, bundles, and transitive resolution are also roadmap features, not
part of the current CLI.
