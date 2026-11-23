---
icon: lucide/settings-2
---

# Configuration

`okr.toml` declares the source context for one project. It is intended to be
reviewed and committed. All paths are resolved relative to the directory that
contains the configuration file, including when `--config` selects a file
outside the current directory.

Unknown keys are hard errors at every level. This makes misspelled settings
fail immediately instead of silently changing reproducibility.

## Complete example

```toml
[project]
r-version = "4.5.1" # optional expected project runtime
snapshot = "2026-06-30"
strict = false
# repo-url = "https://ppm.example.com/cran/2026-06-30" # instead of snapshot

[vendor]
path = "deps-src"
include-tests = true
exclude = []
gitignore = true

[manifest]
agents-file = false

[packages]
rpact = "*"
gsDesign = "3.6.4"
admiral = "pharmaverse/admiral@v1.5.0"
simlib = { git = "git@ghe.example:stats/simlib.git", ref = "v2.1" }
internalpkg = { url = "https://example.com/internalpkg_0.2.1.tar.gz", sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
rtables = { spec = "insightsengineering/rtables@v0.6.13", exclude = ["vignettes/**"] }

[references]
cdisc-standards = "git::git@ghe.example:stds/cdisc.git@2026-Q2"
protocol-templates = { git = "https://codeberg.org/org/protocols.git", ref = "main" }
tamethebeast = { url = "https://mirrors.mit.edu/CTAN/info/bibtex/tamethebeast.zip", sha256 = "4bbdb2d35190426f15d508723600c07e883c67e40fd9f857783c93f3b5130428" }
```

## Project settings

| Key | Default | Meaning |
|---|---|---|
| `r-version` | unset | Exact R runtime expected by the project harness; advisory only. |
| `snapshot` | unset | `YYYY-MM-DD` date of the Posit (Public) Package Manager CRAN snapshot that CRAN packages are resolved from. |
| `strict` | `false` | Make installed-library version mismatches fail `sync`, `status`, and `verify` with exit code 4. |
| `repo-url` | unset | Complete URL of any other CRAN-like repository to resolve CRAN packages from, used exactly as written. Cannot be combined with `snapshot`. |

`r-version` uses R's exact `major.minor.patch` form, such as `4.5.1`. When it
is set, `okr sync` and `okr status` compare it with the version reported by the
`Rscript` on `PATH` and warn when they differ. The field records the runtime
the project or evaluation harness is intended to use; it does not select,
install, or strictly verify R, and it does not affect package source
resolution.

`okr init` runs the `Rscript` on `PATH` read-only from the project directory
and records its version when detection succeeds. If R is absent or unavailable,
initialization still succeeds and omits the optional field. Edit or remove the
generated value when the intended harness differs from the environment used
for initialization. `okr` never substitutes the latest stable R release from
CRAN because that does not describe the project's actual runtime.

## Package repository

CRAN packages are resolved from one repository. A configuration that declares
a CRAN package selects it with exactly one of `snapshot` and `repo-url`; a
remote-only configuration may omit both.

### A dated public snapshot

`snapshot` is the short form for the public default. `snapshot = "2026-06-30"`
reads the repository
`https://packagemanager.posit.co/cran/2026-06-30`.

`okr init` fills `snapshot` with the latest available exact date found in a
bounded 14-day search. It never writes a moving `latest` alias. The date
determines which versions `"*"` CRAN declarations resolve to. To target a
specific snapshot, replace the generated value with the desired `YYYY-MM-DD`
date and run `okr sync` again; `okr` will refresh the lockfile and vendored
sources for the edited configuration.

### Any other repository

Use `repo-url` for a repository that is not a dated public snapshot: a
self-hosted Package Manager, an internal CRAN-like repository, a CRAN mirror,
or a repository on the local filesystem. Set it to the complete repository
URL, which is the value R's `repos` option takes, and remove `snapshot`:

```toml
[project]
repo-url = "https://ppm.example.com/cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7"
```

`okr` uses the URL exactly as written. It reads
`<repo-url>/src/contrib/PACKAGES.gz` and the package archives below
`<repo-url>/src/contrib/`, and it derives nothing from the shape of the URL.
The repository does not have to be dated, and its path does not have to
follow any naming convention:

| Repository | `repo-url` |
|---|---|
| Package Manager snapshot with any identifier | `https://ppm.example.com/cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7` |
| Package Manager URL for a Linux distribution | `https://packagemanager.posit.co/cran/__linux__/noble/2026-06-30` |
| Moving alias | `https://ppm.example.com/internal/latest` |
| CRAN mirror | `https://cloud.r-project.org` |
| Local repository | `file:///srv/cran-mirror` |

The URL must use `https://`, `http://`, or `file://`, and it must be the
repository root rather than its `src/contrib` directory. It cannot contain
whitespace, a query, or a fragment.

`snapshot` and `repo-url` cannot be combined, because `okr` does not append
the snapshot date to the URL. A configuration that sets both is rejected with
the `repo-url` to write instead. For example, `snapshot = "2026-06-30"` with
`repo-url = "https://ppm.example.com/cran"` becomes
`repo-url = "https://ppm.example.com/cran/2026-06-30"`.

### Fixed and moving repositories

A `snapshot` never changes, so `okr` caches its package index and reuses it.

`okr` cannot tell whether a `repo-url` names a fixed snapshot or a repository
that moves, such as a CRAN mirror or a `latest` alias. It therefore reads the
index again whenever an online sync resolves sources, and keeps the most
recent copy in the cache for `--offline`. For a fixed repository this changes
nothing.

For a moving repository, `"*"` resolves to the version the index lists at
that moment. The lock still records the exact version, URL, and tree digest,
and `okr verify` attests them. A later sync that has to resolve again, for
example after a configuration change or in a fresh checkout without the
vendor tree, may select a newer version. When it replaces a version recorded
in an up-to-date lock, `sync` prints a warning and locks the new version. To
keep later syncs on the same versions, point `repo-url` at a fixed snapshot,
declare explicit versions, or rebuild with `okr sync --offline` from the
cache.

## Vendor settings

| Key | Default | Meaning |
|---|---|---|
| `path` | `"deps-src"` | Normalized project-relative destination directory. Absolute paths and `..` are rejected. |
| `include-tests` | `true` | Keep package test suites unless an entry overrides the setting. |
| `exclude` | `[]` | Additional case-insensitive glob patterns applied to every entry. |
| `gitignore` | `true` | Ensure the vendor directory has a root-relative entry in `.gitignore`. |

Package sources receive R-specific default pruning before the additional
exclude patterns are applied. Reference repositories retain everything except
version-control metadata by default. See [source declarations](sources.md#pruning)
for the exact behavior.

Set `gitignore = false` when the vendored sources should be committed, such as
in a sealed benchmark repository. If `init` already added `/deps-src/`, remove
that line from `.gitignore` yourself; disabling management does not delete an
existing entry. The lockfile still makes tree drift detectable.

## Manifest settings

| Key | Default | Meaning |
|---|---|---|
| `agents-file` | `false` | Opt in to a managed pointer to the source manifest in `AGENTS.md`. |

By default, `okr` leaves `AGENTS.md` entirely under project control. Set
`agents-file = true` to maintain an `okr` marker block in that file. Only text
between `<!-- okr:begin -->` and `<!-- okr:end -->` is replaced; existing
instructions outside the block are preserved.

Setting the option back to `false` disables future updates but does not remove
an existing block. Remove the block once after disabling it if it is no longer
wanted.

## Entry values

`[packages]` and `[references]` map a safe directory name to either a string or
a table. Entry names may contain ASCII letters, digits, `.`, `_`, and `-`;
package names must start with a letter. A name cannot occur in both sections.

The table form accepts exactly one of `spec`, `git`, or `url`, where `url`
names an HTTP(S) `.tar.gz`, `.tgz`, or `.zip` archive, plus these optional
keys:

| Key | Meaning |
|---|---|
| `ref` | Branch, tag, or commit, when not already present in `spec`. |
| `sha256` | Required 64-character SHA-256 of the downloaded archive for `url` sources; invalid for other sources. |
| `exclude` | Extra case-insensitive globs for this entry. |
| `include-tests` | Per-package override of `vendor.include-tests`. |

For all supported string forms and examples, see
[source declarations](sources.md).

## Editing with `okr add`

`okr add` uses a format-preserving TOML editor. It retains comments and layout,
creates a missing `[packages]` or `[references]` table, and refuses to replace
an existing entry. Edit table-only options such as a direct URL digest or
per-entry pruning rules in `okr.toml` yourself.

After any declaration or pruning change, run `okr sync` to produce a new lock
and source tree.
