//! R `Remotes`-subset source specification parsing.

use std::fmt;

use crate::{Error, Result};

/// A supported remote source family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteType {
    Github,
    Gitlab,
    Bitbucket,
    Git,
    Url,
}

impl RemoteType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Bitbucket => "bitbucket",
            Self::Git => "git",
            Self::Url => "url",
        }
    }

    const fn is_github(self) -> bool {
        matches!(self, Self::Github)
    }
}

impl fmt::Display for RemoteType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The source location carried by a remote specification.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "shape", rename_all = "kebab-case")]
pub enum RemoteLocation {
    Forge { owner: String, repo: String },
    Git { url: String },
    Url { url: String },
}

/// A named git ref or GitHub's latest-release selector.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteRef {
    Named(String),
    LatestRelease,
}

impl RemoteRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Named(reference) => reference,
            Self::LatestRelease => "*release",
        }
    }
}

/// A validated remote source specification.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteSpec {
    pub remote_type: RemoteType,
    pub location: RemoteLocation,
    pub reference: Option<RemoteRef>,
}

impl RemoteSpec {
    /// Parse a remote specification. A missing type prefix means GitHub.
    pub fn parse(input: &str) -> Result<Self> {
        validate_common(input)?;
        reject_deferred(input)?;

        let (remote_type, body) = match input.split_once("::") {
            Some((kind, body)) => (parse_remote_type(kind)?, body),
            None => (RemoteType::Github, input),
        };

        if body.is_empty() {
            return spec_error("remote specification has an empty body");
        }

        let (body, raw_ref) = split_reference(remote_type, body);
        if body.is_empty() {
            return spec_error("remote specification has an empty body");
        }
        let reference = parse_ref(remote_type, raw_ref)?;

        let location = match remote_type {
            RemoteType::Github | RemoteType::Gitlab | RemoteType::Bitbucket => {
                parse_forge_location(body)?
            }
            RemoteType::Git => {
                validate_location_text(body, "git URL")?;
                RemoteLocation::Git {
                    url: body.to_owned(),
                }
            }
            RemoteType::Url => {
                validate_tarball_url(body)?;
                RemoteLocation::Url {
                    url: body.to_owned(),
                }
            }
        };

        Ok(Self {
            remote_type,
            location,
            reference,
        })
    }

    /// Return the repository/package-shaped name used by `okr add`.
    #[must_use]
    pub fn suggested_name(&self) -> String {
        let raw = match &self.location {
            RemoteLocation::Forge { repo, .. } => repo.as_str(),
            RemoteLocation::Git { url } | RemoteLocation::Url { url } => url
                .trim_end_matches('/')
                .rsplit(['/', ':'])
                .next()
                .unwrap_or(url),
        };
        let raw = raw.strip_suffix(".git").unwrap_or(raw);
        let raw = raw
            .strip_suffix(".tar.gz")
            .or_else(|| raw.strip_suffix(".tgz"))
            .unwrap_or(raw);
        raw.split_once('_').map_or(raw, |(name, _)| name).to_owned()
    }

    #[must_use]
    pub fn ref_name(&self) -> Option<&str> {
        self.reference.as_ref().map(RemoteRef::as_str)
    }
}

impl fmt::Display for RemoteSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}::", self.remote_type)?;
        match &self.location {
            RemoteLocation::Forge { owner, repo } => write!(formatter, "{owner}/{repo}")?,
            RemoteLocation::Git { url } | RemoteLocation::Url { url } => {
                formatter.write_str(url)?;
            }
        }
        if let Some(reference) = &self.reference {
            write!(formatter, "@{}", reference.as_str())?;
        }
        Ok(())
    }
}

/// The unambiguous string form accepted in `[packages]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSpec {
    Cran { version: Option<String> },
    Remote(RemoteSpec),
}

/// Parse a package entry using the slash-or-double-colon disambiguation rule.
pub fn parse_package(input: &str) -> Result<PackageSpec> {
    validate_common(input)?;
    if input.contains('/') || input.contains("::") {
        RemoteSpec::parse(input).map(PackageSpec::Remote)
    } else if input == "*" {
        Ok(PackageSpec::Cran { version: None })
    } else {
        if input.contains(['@', '#']) {
            return spec_error(format!("invalid CRAN version specification `{input}`"));
        }
        Ok(PackageSpec::Cran {
            version: Some(input.to_owned()),
        })
    }
}

/// Parse a reference entry, rejecting the CRAN-shaped string form.
pub fn parse_reference(input: &str) -> Result<RemoteSpec> {
    match parse_package(input)? {
        PackageSpec::Remote(spec) => Ok(spec),
        PackageSpec::Cran { .. } => spec_error(
            "[references] entries must use a git or url source; CRAN specifications are not allowed",
        ),
    }
}

/// A full 40-character hexadecimal commit can bypass `git ls-remote`.
#[must_use]
pub fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_remote_type(kind: &str) -> Result<RemoteType> {
    match kind {
        "github" => Ok(RemoteType::Github),
        "gitlab" => Ok(RemoteType::Gitlab),
        "bitbucket" => Ok(RemoteType::Bitbucket),
        "git" => Ok(RemoteType::Git),
        "url" => Ok(RemoteType::Url),
        "bioc" => spec_error("bioc:: sources are planned for milestone 0.2"),
        "local" => spec_error("local:: sources are planned for milestone 0.2"),
        "svn" => spec_error("svn:: sources are not supported; use git:: or url:: instead"),
        unknown => spec_error(format!(
            "unsupported remote type `{unknown}::`; expected github, gitlab, bitbucket, git, or url"
        )),
    }
}

fn reject_deferred(input: &str) -> Result<()> {
    if looks_like_pull_request(input) {
        return spec_error("pull-request refs (#PR) are planned for milestone 0.2");
    }
    Ok(())
}

fn looks_like_pull_request(input: &str) -> bool {
    let before_ref = input.rsplit_once('@').map_or(input, |(body, _)| body);
    before_ref.rsplit_once('#').is_some_and(|(body, number)| {
        body.contains('/') && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn split_reference(remote_type: RemoteType, body: &str) -> (&str, Option<&str>) {
    let Some((before, reference)) = body.rsplit_once('@') else {
        return (body, None);
    };

    if matches!(
        remote_type,
        RemoteType::Github | RemoteType::Gitlab | RemoteType::Bitbucket
    ) {
        return (before, Some(reference));
    }

    let at_index = before.len();
    if let Some(scheme_index) = body.find("://") {
        let authority_start = scheme_index + 3;
        let authority_end = body[authority_start..]
            .find('/')
            .map_or(body.len(), |offset| authority_start + offset);
        return if at_index >= authority_end {
            (before, Some(reference))
        } else {
            (body, None)
        };
    }

    if let Some(colon_index) = body.find(':')
        && at_index < colon_index
    {
        return (body, None);
    }

    (before, Some(reference))
}

fn parse_ref(remote_type: RemoteType, raw_ref: Option<&str>) -> Result<Option<RemoteRef>> {
    let Some(reference) = raw_ref else {
        return Ok(None);
    };
    if reference.is_empty() {
        return spec_error("remote specification has an empty ref after `@`");
    }
    validate_location_text(reference, "remote ref")?;
    if reference.contains('@') {
        return spec_error("remote refs cannot contain `@`");
    }
    if reference == "*release" {
        if !remote_type.is_github() {
            return spec_error("@*release is supported only for GitHub sources");
        }
        Ok(Some(RemoteRef::LatestRelease))
    } else {
        Ok(Some(RemoteRef::Named(reference.to_owned())))
    }
}

fn parse_forge_location(body: &str) -> Result<RemoteLocation> {
    let mut segments = body.split('/');
    let owner = segments.next().unwrap_or_default();
    let repo = segments.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || segments.next().is_some() {
        return spec_error("forge specifications must have exactly the form `owner/repo`");
    }
    validate_forge_segment(owner, "owner")?;
    validate_forge_segment(repo, "repository")?;
    Ok(RemoteLocation::Forge {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

fn validate_forge_segment(value: &str, label: &str) -> Result<()> {
    if value == "." || value == ".." {
        return spec_error(format!("forge {label} cannot be `{value}`"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return spec_error(format!(
            "forge {label} `{value}` contains unsupported characters"
        ));
    }
    Ok(())
}

fn validate_tarball_url(url: &str) -> Result<()> {
    validate_location_text(url, "tarball URL")?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return spec_error("url:: sources require an http:// or https:// tarball URL");
    }
    let lower = url.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    if !(path.ends_with(".tar.gz") || path.ends_with(".tgz")) {
        return spec_error("url:: sources must point to a .tar.gz or .tgz tarball");
    }
    Ok(())
}

fn validate_common(input: &str) -> Result<()> {
    if input.is_empty() {
        return spec_error("source specification cannot be empty");
    }
    if input.trim() != input {
        return spec_error("source specifications cannot have leading or trailing whitespace");
    }
    if input.chars().any(char::is_control) {
        return spec_error("source specifications cannot contain control characters");
    }
    if input.chars().any(char::is_whitespace) {
        return spec_error("source specifications cannot contain whitespace");
    }
    Ok(())
}

fn validate_location_text(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        return spec_error(format!("{label} cannot be empty"));
    }
    if value.chars().any(char::is_control) || value.chars().any(char::is_whitespace) {
        return spec_error(format!(
            "{label} cannot contain whitespace or control characters"
        ));
    }
    Ok(())
}

fn spec_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Spec(message.into()))
}

#[cfg(test)]
mod tests {
    use super::{
        PackageSpec, RemoteLocation, RemoteRef, RemoteSpec, RemoteType, is_full_commit_sha,
        parse_package, parse_reference,
    };

    struct ValidCase {
        input: &'static str,
        remote_type: RemoteType,
        location: RemoteLocation,
        reference: Option<RemoteRef>,
    }

    #[test]
    fn parses_every_supported_spec_table_row() {
        let cases = [
            ValidCase {
                input: "pharmaverse/admiral@v1.5.0",
                remote_type: RemoteType::Github,
                location: forge("pharmaverse", "admiral"),
                reference: named("v1.5.0"),
            },
            ValidCase {
                input: "github::tidyverse/ggplot2",
                remote_type: RemoteType::Github,
                location: forge("tidyverse", "ggplot2"),
                reference: None,
            },
            ValidCase {
                input: "r-lib/testthat@*release",
                remote_type: RemoteType::Github,
                location: forge("r-lib", "testthat"),
                reference: Some(RemoteRef::LatestRelease),
            },
            ValidCase {
                input: "gitlab::jimhester/covr@abc123",
                remote_type: RemoteType::Gitlab,
                location: forge("jimhester", "covr"),
                reference: named("abc123"),
            },
            ValidCase {
                input: "bitbucket::sulab/mygene.r@default",
                remote_type: RemoteType::Bitbucket,
                location: forge("sulab", "mygene.r"),
                reference: named("default"),
            },
            ValidCase {
                input: "git::git@ghe.corp.example:stats/simlib.git@v2.1",
                remote_type: RemoteType::Git,
                location: git("git@ghe.corp.example:stats/simlib.git"),
                reference: named("v2.1"),
            },
            ValidCase {
                input: "git::https://codeberg.org/org/pkg.git@v1.0",
                remote_type: RemoteType::Git,
                location: git("https://codeberg.org/org/pkg.git"),
                reference: named("v1.0"),
            },
            ValidCase {
                input: "url::https://example.com/pkg_0.2.1.tar.gz",
                remote_type: RemoteType::Url,
                location: url("https://example.com/pkg_0.2.1.tar.gz"),
                reference: None,
            },
        ];

        for case in cases {
            let parsed = RemoteSpec::parse(case.input)
                .unwrap_or_else(|error| panic!("failed to parse `{}`: {error}", case.input));
            assert_eq!(parsed.remote_type, case.remote_type, "{}", case.input);
            assert_eq!(parsed.location, case.location, "{}", case.input);
            assert_eq!(parsed.reference, case.reference, "{}", case.input);
        }
    }

    #[test]
    fn every_deferred_or_rejected_table_row_is_instructive() {
        let cases = [
            ("bioc::BiocGenerics", "milestone 0.2"),
            ("local::../mypkg", "milestone 0.2"),
            ("owner/repo#123", "milestone 0.2"),
            ("svn::https://example.com/repo", "git:: or url::"),
        ];

        for (input, guidance) in cases {
            let error = RemoteSpec::parse(input).unwrap_err();
            assert!(
                error.to_string().contains(guidance),
                "`{input}` error `{error}` did not contain `{guidance}`"
            );
        }
    }

    #[test]
    fn package_disambiguation_is_syntactic_not_a_version_solver() {
        assert_eq!(
            parse_package("*").unwrap(),
            PackageSpec::Cran { version: None }
        );
        assert_eq!(
            parse_package("3.6.4").unwrap(),
            PackageSpec::Cran {
                version: Some("3.6.4".into())
            }
        );
        assert!(matches!(
            parse_package("org/repo").unwrap(),
            PackageSpec::Remote(_)
        ));
        assert!(matches!(
            parse_package("git::host:path").unwrap(),
            PackageSpec::Remote(_)
        ));
    }

    #[test]
    fn references_reject_cran_strings() {
        let error = parse_reference("1.2.3").unwrap_err();
        assert!(error.to_string().contains("[references]"));
        assert!(error.to_string().contains("CRAN"));
    }

    #[test]
    fn scp_and_authenticated_urls_do_not_confuse_user_at_with_ref() {
        let scp = RemoteSpec::parse("git::git@example.test:org/repo.git").unwrap();
        assert_eq!(scp.ref_name(), None);
        assert_eq!(scp.location, git("git@example.test:org/repo.git"));

        let authenticated =
            RemoteSpec::parse("git::https://user@example.test/org/repo.git").unwrap();
        assert_eq!(authenticated.ref_name(), None);
        assert_eq!(
            authenticated.location,
            git("https://user@example.test/org/repo.git")
        );
    }

    #[test]
    fn branch_names_may_contain_slashes() {
        let parsed = RemoteSpec::parse("github::owner/repo@feature/parser").unwrap();
        assert_eq!(parsed.ref_name(), Some("feature/parser"));
    }

    #[test]
    fn latest_release_is_github_only() {
        for input in [
            "gitlab::owner/repo@*release",
            "bitbucket::owner/repo@*release",
            "git::https://example.test/repo.git@*release",
        ] {
            let error = RemoteSpec::parse(input).unwrap_err();
            assert!(error.to_string().contains("only for GitHub"));
        }
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        let cases = [
            "",
            " owner/repo",
            "owner/repo ",
            "owner /repo",
            "github::",
            "owner",
            "owner/",
            "/repo",
            "owner/repo/extra",
            "owner/repo@",
            "owner/repo#not-a-pr",
            "github::../repo",
            "github::owner/..",
            "github::owner/repo\nmain",
            "hg::owner/repo",
            "url::file:///tmp/pkg.tar.gz",
            "url::https://example.com/pkg.zip",
        ];

        for input in cases {
            assert!(
                RemoteSpec::parse(input).is_err(),
                "`{input}` unexpectedly parsed"
            );
        }
    }

    #[test]
    fn full_sha_detection_is_exact() {
        assert!(is_full_commit_sha(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(is_full_commit_sha(
            "0123456789ABCDEF0123456789ABCDEF01234567"
        ));
        assert!(!is_full_commit_sha("0123456"));
        assert!(!is_full_commit_sha(
            "g123456789abcdef0123456789abcdef01234567"
        ));
    }

    #[test]
    fn canonical_display_is_explicit() {
        let parsed = RemoteSpec::parse("org/repo@main").unwrap();
        assert_eq!(parsed.to_string(), "github::org/repo@main");
    }

    #[test]
    fn suggested_names_cover_all_location_shapes() {
        assert_eq!(
            RemoteSpec::parse("org/repo@main").unwrap().suggested_name(),
            "repo"
        );
        assert_eq!(
            RemoteSpec::parse("git::ssh://host/org/repo.git")
                .unwrap()
                .suggested_name(),
            "repo"
        );
        assert_eq!(
            RemoteSpec::parse("url::https://host/pkg_1.2.3.tar.gz")
                .unwrap()
                .suggested_name(),
            "pkg"
        );
    }

    fn forge(owner: &str, repo: &str) -> RemoteLocation {
        RemoteLocation::Forge {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    fn git(value: &str) -> RemoteLocation {
        RemoteLocation::Git { url: value.into() }
    }

    fn url(value: &str) -> RemoteLocation {
        RemoteLocation::Url { url: value.into() }
    }

    fn named(value: &str) -> Option<RemoteRef> {
        Some(RemoteRef::Named(value.into()))
    }
}
