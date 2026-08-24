---
icon: lucide/fingerprint
---

# Integrity and reproducibility

`okr` attests two different byte sets: the acquired artifact and the pruned
source tree that an agent reads. Both matter.

## Artifact digest

`artifact-digest` identifies the downloaded archive or normalized clone archive
stored in the cache. Direct `url` declarations require this digest up front;
other sources acquire it during sync.

The artifact digest protects transfer and cache integrity, but it cannot attest
the result of extraction and pruning.

## Digest naming and values

Every serialized attestation field uses a `<subject>-digest` name and an
algorithm-tagged value such as `sha256:<lowercase-hex>`. The lock therefore
uses `config-digest`, `environment-digest`, `artifact-digest`, and
`tree-digest`. The subject says which bytes are covered; the value says how
they were digested. TOML keys use kebab case; their JSON manifest equivalents
use snake_case (`environment_digest`, `artifact_digest`, and `tree_digest`).

The `sha256` configuration key for a direct URL is intentionally
algorithm-specific because it is an input pin. A git `commit` is a
source control identifier rather than an okr content digest.

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

## Environment digest

The environment digest hashes the lock schema and the sorted package and
reference records, including every aggregate tree digest. It is the compact
stamp an evaluation harness can record for a run.

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

## What `verify` checks

`okr verify` recomputes the complete vendor state and checks:

- lock schema and sorted entry order;
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
