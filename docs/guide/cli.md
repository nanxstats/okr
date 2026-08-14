---
icon: lucide/terminal
---

# CLI reference

Run `okr --help` or `okr <COMMAND> --help` for the help bundled with your
installed version.

```text
okr [GLOBAL OPTIONS] <COMMAND>
```

## Global options

| Option | Meaning |
|---|---|
| `--config <PATH>` | Configuration path; defaults to `okr.toml`. The file's parent is the project directory. |
| `--quiet` | Suppress non-error human output. Conflicts with `--verbose`. |
| `--verbose` | Print additional sync diagnostics, including the environment digest and cache summary. |
| `--json` | Emit schema-versioned JSON for `status` or `verify`. Rejected by other commands. |
| `-h`, `--help` | Show help. |
| `-V`, `--version` | Show the installed version. |

## Commands

### `okr init [--force]`

Creates a default configuration and manages its `.gitignore` entry. It probes
the current UTC date and up to 13 preceding dates for the first available
snapshot, caching the successful `PACKAGES.gz` response.

`--force` replaces an existing configuration. The displayed `--profile`
option is reserved for milestone 0.2 and currently returns an instructive
error.

### `okr add <SPEC>... [--reference]`

Adds one or more declarations without synchronizing. Bare names are CRAN
packages; remote forms follow the grammar in
[source declarations](sources.md). `--reference` writes to `[references]`
instead of `[packages]`.

Existing entries are never overwritten, and edits preserve TOML comments and
formatting. Direct URL sources require a SHA-256 and therefore must be written
in table form in `okr.toml`.

### `okr sync [--offline] [--strict]`

Converges the project in five stages: resolve, acquire, vendor, lock and
manifest, then diagnose the installed R library. `--offline` prohibits all
network and clone operations and requires prior lock resolutions and cache
hits. `--strict` upgrades installed-library mismatches to exit code 4.

An unchanged configuration with an intact vendor tree uses a digest-based
no-op path. Interactive terminals show progress; redirected, quiet, and
non-interactive output remains line-oriented.

### `okr status [--json]`

Reports:

- R availability, version, and advisory `project.r-version` agreement;
- `git` and `gh` availability;
- lock presence and configuration freshness;
- vendor-tree status and mismatch count;
- installed-library coherence;
- cache path, artifact count, and size; and
- a copy-paste package installation command when packages are locked.

Status never executes the installation command. With `project.strict = true`,
an installed-library mismatch exits 4.

### `okr verify [--json] [--strict]`

Rehashes the full vendored context against `okr.lock`, including generated
manifests. Tree drift is always fatal. `--strict`, or `project.strict = true`,
also checks the installed R library and makes coherence mismatches fatal.

The JSON response contains `schema`, `ok`, `environment_digest`, `mismatches`,
and optional `coherence` fields. Human-readable verification prints the
environment digest on success.

## Exit codes

| Code | Class |
|---:|---|
| `0` | Success. |
| `1` | Unexpected I/O or internal error. |
| `2` | Configuration or source-specification error. |
| `3` | Network, acquisition, or offline cache-miss error. |
| `4` | Integrity verification or strict installed-library coherence failure. |
