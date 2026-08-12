# Test fixtures

These fixtures are synthetic and contain no live-network dependencies. The
`fixture-repo` directory mimics a dated CRAN/PPM `src/contrib` tree; `forge`
uses a commit-shaped top-level archive directory; and `reference-repo` is a
non-package git fixture whose `.gitattributes` exercises the documented
`export-ignore` fetch-method caveat.
