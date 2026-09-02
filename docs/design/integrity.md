---
icon: lucide/fingerprint
---

# Integrity and reproducibility

`okr` attests one byte set: the pruned source tree that an agent reads. The
lock records exactly three kinds of digest, each with a consumer.

| Digest | Covers | Used by |
|---|---|---|
| `config-digest` | The normalized `okr.toml` model | `sync` and `status`, to detect a stale lock |
| `environment-digest` | The lock format version and every sorted entry | Evaluation harnesses and `verify` |
| `tree-digest` | The files of one vendored entry after pruning | `verify`, and `sync` when it rebuilds a locked source |

## Digest naming and values

Every serialized attestation field uses a `<subject>-digest` name and an
algorithm-tagged value such as `sha256:<lowercase-hex>`. The subject says which
bytes are covered; the value says how they were digested. TOML keys use kebab
case; their JSON manifest equivalents use snake_case (`environment_digest` and
`tree_digest`).

The `sha256` configuration key for a direct URL is intentionally
algorithm-specific because it is an input pin. A git `commit` is a
source control identifier rather than an okr content digest.

## Why there is no artifact digest

Earlier lock formats also recorded an `artifact-digest` for the downloaded
archive or normalized clone archive. It was dropped because it had no job the
other fields do not already do.

Every source has an upstream identity without it: a CRAN package is fixed by
its snapshot, version, and URL; a Git source by its full commit; and a direct
`url` source by the `sha256` pin declared in `okr.toml`, which is still checked
before the download is committed to the cache. The tree digest catches every
change to the bytes agents read. An archive digest, by contrast, pins bytes
that forges do not promise to keep stable, so it could fail while every
vendored file was identical.

Offline replay does not need it either. The cache keys each artifact by its
fetch method and source, and the locked `fetch-method` selects which key to
replay.

This is the same choice Go makes in `go.sum`, which hashes a module's file
tree rather than its zip, and the choice uv and Cargo make by recording no
hash for Git sources.

## Tree digest

For each vendored entry, `okr` recursively inventories regular files. Paths are
UTF-8, normalized to `/`, and sorted. Every record has this form:

```text
relative/path<TAB>sha256-of-exact-file-bytes
```

The records are joined with LF and no trailing newline, then hashed again with
SHA-256. The resulting `tree-digest` covers all source bytes exposed to the agent.
Archive and clone symbolic links are normalized to regular files containing
their link-target bytes before the inventory is built. Symlinks introduced
later and other non-files are rejected rather than hashed.

Only the aggregate digest is serialized. Per-file hashes are internal inputs,
so lockfiles and manifests remain compact as source trees grow. A verification
failure therefore identifies the changed entry and aggregate digest rather than
claiming a stored per-file diagnosis.

When `sync` rebuilds an entry that a fresh lock from the same okr release
already records, the rebuilt tree must reproduce the locked tree digest. A
difference means the locked source changed upstream and is reported as a fetch
error naming the entry. If the change is intended, delete `okr.lock` and sync
again to lock the new content.

## Environment digest

The environment digest hashes the lock format version and the sorted package
and reference records, including every aggregate tree digest. It is the compact
stamp an evaluation harness can record for a run. It is derived from the rest
of the lock, but it is recorded so a harness can read it without running okr
and so `verify` can detect a manually edited lock.

Other deterministic lock properties include:

- packages and references sorted by name within their kind;
- `generated` set to snapshot midnight, or the Unix epoch for a remote-only
  lock, rather than wall-clock time;
- a normalized configuration digest for staleness detection; and
- stable TOML serialization.

Comments and presentation changes in `okr.toml` do not affect its normalized
configuration digest. Behavioral configuration changes do.

## Why fetch method is provenance

A forge-generated archive may honor `.gitattributes` rules such as
`export-ignore` and `export-subst`; a Git checkout of the same commit may not
produce identical files. The lock therefore records one of `tarball`,
`forge-tarball`, `gh`, or `git-clone`.

Reproduction replays the cached artifact corresponding to the locked method.
It does not silently switch methods and assume the same commit implies the same
tree.

## Lock format version

`okr.lock` starts with a `version` key. Each okr release reads and writes one
lock format version. When `sync` finds a lock written in an older format, it
regenerates the lock and says so; `status` and `verify` report the older
version and ask for `okr sync`. The current version is 2. Version 1 also
recorded an artifact digest per entry.

## What `verify` checks

`okr verify` recomputes the complete vendor state and checks:

- lock format version and sorted entry order;
- the lock's environment digest;
- every package and reference tree digest;
- missing or unexpected vendor entries; and
- the exact generated Markdown and JSON manifests.

Any tree or generated-file drift exits with code 4, independent of strictness.
`status` reports whether the current normalized configuration digest still
matches the lock, and `sync` creates a new lock whenever it does not.
`--strict` adds a read-only comparison between locked package versions and the
installed R library. R absence is a successful skip; a mismatch when inspection
succeeds is a strict failure.
