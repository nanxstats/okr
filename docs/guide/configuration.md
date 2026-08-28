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
# repo-url = "https://packagemanager.posit.co/cran"

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
```

## Project settings

| Key | Default | Meaning |
|---|---|---|
| `r-version` | unset | Exact R runtime expected by the project harness; advisory only. |
| `snapshot` | unset | Exact `YYYY-MM-DD` CRAN snapshot. Required when any CRAN package is declared. |
| `strict` | `false` | Make installed-library version mismatches fail `sync`, `status`, and `verify` with exit code 4. |
| `repo-url` | Posit (Public) Package Manager CRAN | Base URL for a compatible dated CRAN repository or mirror. |

`okr init` fills `snapshot` with the latest available exact date found in a
bounded 14-day search. It never writes a moving `latest` alias. A remote-only
configuration may omit the snapshot. The date determines which versions `"*"`
CRAN declarations resolve to. To target a specific snapshot, replace the
generated value with the desired `YYYY-MM-DD` date and run `okr sync` again;
`okr` will refresh the lockfile and vendored sources for the edited
configuration.

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

The table form accepts exactly one of `spec`, `git`, or `url`, plus these
optional keys:

| Key | Meaning |
|---|---|
| `ref` | Branch, tag, or commit, when not already present in `spec`. |
| `sha256` | Required 64-character artifact digest for `url` sources; invalid for other sources. |
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
