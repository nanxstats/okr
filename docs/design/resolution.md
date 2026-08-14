---
icon: lucide/network
---

# Resolution and acquisition

Resolution freezes a declaration to an exact source plan. Acquisition obtains
the bytes for that plan. `okr` keeps the two concerns separate so the lock can
record both what was selected and how its source tree was produced.

## CRAN lookup

For CRAN entries, `okr` reads the configured dated repository's
`src/contrib/PACKAGES.gz` once per sync. A wildcard selects the indexed version;
an explicit version is validated against the index and falls back to its CRAN
archive URL when superseded.

When an index contains repeated records for one package, same-version records
are accepted. If repeated records disagree, `okr` deterministically selects the
greatest version using R's numeric package-version ordering.

This is deliberately a metadata lookup. `okr` does not inspect `Imports`, solve
constraints, or add undeclared packages.

## Freezing Git refs

The resolution path depends on the declaration:

| Declaration | Online resolution |
|---|---|
| Full 40-character commit | Used directly; no ref lookup. |
| Named tag, branch, or abbreviated commit | `git ls-remote`, with a public-forge API fallback where available. |
| Omitted ref | Resolve the remote default branch and warn that it is movable. |
| GitHub `@*release` | Resolve the latest release through `gh`, `GITHUB_TOKEN`, or anonymous REST. |
| Offline remote | Reuse the matching commit from the prior lock. |

Regardless of the input form, the completed lock records the full commit. A
branch warning is about the next online resolution, not ambiguity in the
current lock.

## Choosing an acquisition method

`okr` uses the fastest applicable authenticated method and records the actual
choice as provenance.

| Source situation | Typical method |
|---|---|
| CRAN or direct URL | Verified HTTP tarball |
| Public GitHub, GitLab, Bitbucket, or Codeberg repository | Forge archive at the resolved commit |
| Private GitHub repository | Authenticated `gh` archive download when available |
| Arbitrary or self-hosted Git remote | Shallow Git clone, with `core.autocrlf=false` |
| Archive download that cannot be used | Git clone fallback |

Clone fallback verifies that `HEAD` equals the resolved commit and removes Git
metadata. The user's Git configuration, SSH keys, and credential helpers remain
in control; `okr` has no credential database.

## Cache and offline replay

HTTP artifacts enter the content-addressed cache only after their SHA-256 has
been verified. Hits are reverified before use and files are committed through a
temporary-file rename.

A clone-produced tree has no upstream tarball to replay. After pruning it,
`okr` creates a normalized gzip tarball with sorted paths, zero timestamps and
ownership, and normalized modes. That archive becomes the cached artifact for
later offline syncs.

Offline mode permits only prior lock resolutions and verified cache hits. It
never silently changes from an archive to a clone or contacts a host to refresh
a ref.

## Safe extraction and replacement

Archives must contain regular files below one safe top-level directory. Absolute
paths, `..` traversal, links, and special files are rejected. PAX global
metadata records are ignored when detecting the archive's top-level directory.

The extracted tree is pruned according to its package or reference kind, then
written through a sibling temporary directory and renamed into place. A failed
sync does not intentionally expose a half-written entry as a successful result.
