---
icon: lucide/download
---

# Install

Install with Homebrew:

```console
brew install nanxstats/tap/okr
```

Alternatively, install with Cargo:

```console
cargo install okr
```

Building from source requires Rust 1.88 or newer. To install the development
version from the main branch:

```console
cargo install --git https://github.com/nanxstats/okr.git
```

Confirm the installation:

```console
$ okr --version
okr 0.1.4
```

## Runtime tools

The binary has no mandatory R or Git runtime dependency for ordinary CRAN and
HTTP archive sources.

| Tool or setting | When it is used |
|---|---|
| `Rscript` | Optional, read-only inspection of the R version and installed packages. |
| `git` | Ref resolution and clone fallback for Git sources, especially arbitrary, private, and self-hosted remotes. |
| `gh` | Optional authenticated GitHub release lookup and private archive download. |
| `GITHUB_TOKEN` | GitHub API authentication when authenticated `gh` is unavailable. |

Public-forge API fallbacks allow many GitHub, GitLab, Bitbucket, and Codeberg
sources to work without `git`. SSH URLs and private repositories use your
existing Git or `gh` authentication; `okr` does not store credentials.

Run `okr status` in an initialized project to see whether `Rscript`, `git`, and
`gh` are available.

## Cache location

Downloaded and clone-derived artifacts are stored in a content-addressed cache
at `~/.cache/okr` by default. Set `OKR_CACHE_DIR` before running `okr` to use a
different location:

```console
export OKR_CACHE_DIR=/path/to/okr-cache
```
