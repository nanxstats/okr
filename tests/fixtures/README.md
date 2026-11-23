# Test fixtures

These fixtures are synthetic and contain no live-network dependencies. The
`fixture-repo/2026-06-30` directory is the root of a CRAN-like repository with
a `src/contrib` tree; `forge`
uses a commit-shaped top-level archive directory; `zip` holds an Info-ZIP
archive of a non-package reference tree with directory entries and a mix of
stored and deflated members, built from `zip/source` with
`zip -X -r ../reference-archive.zip notes-1.0`; and `reference-repo` is a
non-package git fixture whose `.gitattributes` exercises the documented
`export-ignore` fetch-method caveat.
