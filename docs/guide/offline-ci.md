---
icon: lucide/shield-check
---

# Offline and CI workflows

`okr` separates acquisition from verification. Run an online sync once to
populate the content-addressed cache, then rebuild or attest the same source
context without network access.

## Warm the cache online

```console
export OKR_CACHE_DIR=/srv/okr-cache
okr sync
```

The cache stores downloaded archives by SHA-256 and indexes them by source, so
a locked entry is replayed through its recorded fetch method. A source acquired
by cloning is pruned and converted to a normalized archive so it can also be
replayed offline. Cache hits are rehashed before use.

Keep `okr.toml`, `okr.lock`, and the cache together when preparing an offline
environment. The prior lock supplies frozen commits for remote declarations;
the cache supplies their exact artifacts.

## Rebuild without network access

```console
export OKR_CACHE_DIR=/srv/okr-cache
okr sync --offline
```

Offline mode does not download, query an API, run `git ls-remote`, or clone. A
missing lock resolution or cache artifact is a fetch failure with exit code 3
and an error naming what must first be acquired online.

The vendor directory itself may be removed before the offline sync if the lock
and all required cached artifacts remain available. The rebuilt tree must
reproduce every tree digest in the lock; a difference is reported as a fetch
error naming the entry.

## Verify an existing tree

```console
okr verify --json
```

`verify` needs neither the network nor the artifact cache. It recomputes every
entry's aggregate digest, checks the generated manifests and lock invariants,
and exits 4 on drift. The JSON report includes a schema number, the environment
digest, and entry-level mismatches.

Use strict verification when the installed R library is also part of the
attested environment:

```console
okr verify --strict --json
```

The R inspection is read-only. If `Rscript` is absent, coherence is reported as
skipped rather than causing an installation attempt. When R is present, a
package that is missing from the library or installed at a different version
is a strict failure.

## CI pattern

A source context integrity job can be as small as:

```yaml
- name: Verify vendored R source context
  run: okr verify --json
```

Choose what to commit based on the environment:

| File or directory | Recommended treatment |
|---|---|
| `okr.toml` | Commit; this is the reviewed declaration. |
| `okr.lock` | Commit; this freezes provenance and digests. |
| `deps-src/` | Ignored by default; commit it when an evaluation repository must be self-contained. |
| Artifact cache | Keep in image or CI cache storage; do not treat it as the declaration of record. |

For an evaluation image, a common sequence is:

```console
# Network-enabled image build
okr sync

# Network-disabled evaluation startup
okr verify --strict --json
```

## Container builds

Deterministic portable bundles are planned for milestone 0.2. In the current
release, carry the lock, vendor tree, and cache through your own image or
artifact workflow.

`okr` deliberately does not discover OS system packages or generate a
Dockerfile. Those decisions belong to the tool that constructs the runnable R
environment and to the container build itself. Use `renv::sysreqs()` or
`pak::pkg_sysreqs()` to identify system dependencies, and follow
renv's Docker workflow for package restoration and image layout.

The okr artifacts remain composable inputs to that Dockerfile: build or copy
the vendored source context into the image, then run `okr verify` as its
integrity gate. This keeps container policy outside okr without weakening the
source context attestation.
