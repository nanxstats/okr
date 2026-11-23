//! Strict project configuration models and validation.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};

use crate::spec::{PackageSpec, RemoteLocation, RemoteSpec, parse_package};
use crate::{Error, Result};

/// The Posit Public Package Manager CRAN repository whose dated snapshots
/// `project.snapshot` selects.
pub const SNAPSHOT_BASE_URL: &str = "https://packagemanager.posit.co/cran";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub vendor: VendorConfig,
    #[serde(default)]
    pub manifest: ManifestConfig,
    #[serde(default)]
    pub packages: BTreeMap<String, EntryValue>,
    #[serde(default)]
    pub references: BTreeMap<String, EntryValue>,
}

impl Config {
    pub fn parse(input: &str) -> Result<Self> {
        let config: Self = toml::from_str(input)
            .map_err(|error| Error::Config(format!("invalid okr.toml: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let input = fs::read_to_string(path).map_err(|error| {
            Error::Config(format!(
                "could not read configuration {}: {error}",
                path.display()
            ))
        })?;
        Self::parse(&input)
    }

    pub fn validate(&self) -> Result<()> {
        validate_vendor_path(&self.vendor.path)?;
        validate_globs(&self.vendor.exclude, "vendor.exclude")?;
        if let Some(r_version) = &self.project.r_version {
            validate_r_version(r_version)?;
        }
        if let Some(snapshot) = &self.project.snapshot {
            validate_snapshot(snapshot)?;
        }
        if let Some(repo_url) = &self.project.repo_url {
            validate_repo_url(repo_url)?;
        }
        if let (Some(snapshot), Some(repo_url)) = (&self.project.snapshot, &self.project.repo_url) {
            return Err(Error::Config(format!(
                "project.snapshot and project.repo-url cannot both be set: `repo-url` is the complete repository URL and is used exactly as written, so okr does not append the snapshot to it. Remove `snapshot`; if the repository is dated, keep the date in the URL, for example `repo-url = \"{}/{snapshot}\"`",
                repo_url.trim_end_matches('/')
            )));
        }

        let mut has_cran = false;
        for (name, value) in &self.packages {
            validate_entry_name(name, EntryKind::Package)?;
            let entry = value.declare(name, EntryKind::Package)?;
            validate_globs(&entry.exclude, &format!("[packages].{name}.exclude"))?;
            has_cran |= matches!(entry.source, DeclaredSource::Cran { .. });
        }
        for (name, value) in &self.references {
            validate_entry_name(name, EntryKind::Reference)?;
            if self.packages.contains_key(name) {
                return Err(Error::Config(format!(
                    "entry name `{name}` appears in both [packages] and [references]"
                )));
            }
            let entry = value.declare(name, EntryKind::Reference)?;
            validate_globs(&entry.exclude, &format!("[references].{name}.exclude"))?;
        }

        if has_cran && self.repository().is_none() {
            return Err(Error::Config(
                "project.snapshot or project.repo-url is required when [packages] contains a CRAN entry"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn declared_entries(&self) -> Result<Vec<DeclaredEntry>> {
        let mut entries = Vec::with_capacity(self.packages.len() + self.references.len());
        for (name, value) in &self.packages {
            entries.push(value.declare(name, EntryKind::Package)?);
        }
        for (name, value) in &self.references {
            entries.push(value.declare(name, EntryKind::Reference)?);
        }
        Ok(entries)
    }

    /// The repository that CRAN declarations resolve against, when the
    /// project configures one.
    #[must_use]
    pub fn repository(&self) -> Option<Repository> {
        match (&self.project.repo_url, &self.project.snapshot) {
            (Some(url), _) => Some(Repository::at(url)),
            (None, Some(date)) => Some(Repository::snapshot(date)),
            (None, None) => None,
        }
    }
}

/// A CRAN-like package repository: a root URL that serves
/// `src/contrib/PACKAGES.gz` and the source packages it lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    url: String,
    snapshot: Option<String>,
}

impl Repository {
    /// The dated Posit Public Package Manager CRAN snapshot for `date`.
    #[must_use]
    pub fn snapshot(date: &str) -> Self {
        Self {
            url: format!("{SNAPSHOT_BASE_URL}/{date}"),
            snapshot: Some(date.to_owned()),
        }
    }

    /// The repository at `url`. The URL is used exactly as written: nothing
    /// is appended to it but `src/contrib` paths, and nothing is inferred
    /// from its shape.
    #[must_use]
    pub fn at(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            snapshot: None,
        }
    }

    /// The repository root, which is the URL that R's `repos` option takes.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The date of the snapshot, when `project.snapshot` selected this
    /// repository.
    #[must_use]
    pub fn snapshot_date(&self) -> Option<&str> {
        self.snapshot.as_deref()
    }

    /// Whether the contents of the repository are known never to change.
    /// Only a dated snapshot makes that promise; a `repo-url` may name a
    /// fixed snapshot or a moving repository, and okr cannot tell which.
    #[must_use]
    pub const fn is_fixed(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The URL of the index of the source packages in the repository.
    #[must_use]
    pub fn index_url(&self) -> String {
        format!("{}/src/contrib/PACKAGES.gz", self.url)
    }
}

impl std::fmt::Display for Repository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.snapshot {
            Some(date) => write!(formatter, "snapshot {date}"),
            None => write!(formatter, "the repository at {}", self.url),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub r_version: Option<String>,
    /// The `YYYY-MM-DD` date of a Posit Public Package Manager CRAN snapshot.
    pub snapshot: Option<String>,
    pub strict: bool,
    /// The complete URL of a CRAN-like repository to use instead of a
    /// `snapshot`. The two keys are mutually exclusive.
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct VendorConfig {
    pub path: PathBuf,
    pub include_tests: bool,
    pub exclude: Vec<String>,
    pub gitignore: bool,
}

impl Default for VendorConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("deps-src"),
            include_tests: true,
            exclude: Vec::new(),
            gitignore: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ManifestConfig {
    pub agents_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntryValue {
    String(String),
    Table(EntryTable),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct EntryTable {
    pub spec: Option<String>,
    pub git: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub sha256: Option<String>,
    pub exclude: Vec<String>,
    pub include_tests: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Package,
    Reference,
}

impl EntryKind {
    #[must_use]
    pub const fn section(self) -> &'static str {
        match self {
            Self::Package => "packages",
            Self::Reference => "references",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredEntry {
    pub name: String,
    pub kind: EntryKind,
    pub source: DeclaredSource,
    pub exclude: Vec<String>,
    pub include_tests: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredSource {
    Cran {
        requested_version: Option<String>,
    },
    Remote {
        spec: RemoteSpec,
        expected_sha256: Option<String>,
    },
}

impl EntryValue {
    pub fn declare(&self, name: &str, kind: EntryKind) -> Result<DeclaredEntry> {
        let (raw_spec, sha256, exclude, include_tests, reference) = match self {
            Self::String(spec) => (Cow::Borrowed(spec.as_str()), None, Vec::new(), None, None),
            Self::Table(table) => {
                let selected = [
                    table.spec.as_deref(),
                    table.git.as_deref(),
                    table.url.as_deref(),
                ]
                .into_iter()
                .flatten()
                .count();
                if selected != 1 {
                    return config_entry_error(
                        kind,
                        name,
                        "table form requires exactly one of `spec`, `git`, or `url`",
                    );
                }
                let raw = if let Some(spec) = &table.spec {
                    spec.clone()
                } else if let Some(git) = &table.git {
                    format!("git::{git}")
                } else {
                    format!("url::{}", table.url.as_deref().unwrap_or_default())
                };
                (
                    Cow::Owned(raw),
                    table.sha256.as_deref(),
                    table.exclude.clone(),
                    table.include_tests,
                    table.reference.as_deref(),
                )
            }
        };

        let mut parsed = parse_package(raw_spec.as_ref())
            .map_err(|error| contextualize_entry_error(error, kind, name))?;
        if let Some(table_ref) = reference {
            if table_ref.is_empty() {
                return config_entry_error(kind, name, "`ref` cannot be empty");
            }
            match &mut parsed {
                PackageSpec::Cran { .. } => {
                    return config_entry_error(kind, name, "`ref` cannot be used with CRAN");
                }
                PackageSpec::Remote(spec) if spec.reference.is_some() => {
                    return config_entry_error(
                        kind,
                        name,
                        "the source specification and table must not both set `ref`",
                    );
                }
                PackageSpec::Remote(spec) => {
                    let reparsed = RemoteSpec::parse(&format!("{spec}@{table_ref}"))
                        .map_err(|error| contextualize_entry_error(error, kind, name))?;
                    *spec = reparsed;
                }
            }
        }

        let source = match parsed {
            PackageSpec::Cran { version } => {
                if kind == EntryKind::Reference {
                    return config_entry_error(
                        kind,
                        name,
                        "CRAN specifications are not allowed; use a git or url source",
                    );
                }
                if sha256.is_some() {
                    return config_entry_error(
                        kind,
                        name,
                        "`sha256` is only valid for url sources",
                    );
                }
                DeclaredSource::Cran {
                    requested_version: version,
                }
            }
            PackageSpec::Remote(spec) => {
                let is_url = matches!(spec.location, RemoteLocation::Url { .. });
                if is_url && sha256.is_none() {
                    return config_entry_error(
                        kind,
                        name,
                        "url sources require table form with a `sha256` value",
                    );
                }
                if !is_url && sha256.is_some() {
                    return config_entry_error(
                        kind,
                        name,
                        "`sha256` is only valid for url sources",
                    );
                }
                if let Some(digest) = sha256 {
                    validate_sha256(digest).map_err(|message| {
                        Error::Config(format!("[{}].{name}: {message}", kind.section()))
                    })?;
                }
                DeclaredSource::Remote {
                    spec,
                    expected_sha256: sha256.map(str::to_ascii_lowercase),
                }
            }
        };

        Ok(DeclaredEntry {
            name: name.to_owned(),
            kind,
            source,
            exclude,
            include_tests,
        })
    }
}

fn validate_entry_name(name: &str, kind: EntryKind) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        return config_entry_error(kind, name, "entry name is not a safe directory name");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return config_entry_error(
            kind,
            name,
            "entry names may contain only ASCII letters, digits, `.`, `_`, and `-`",
        );
    }
    if kind == EntryKind::Package && !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return config_entry_error(kind, name, "R package names must start with a letter");
    }
    Ok(())
}

fn validate_snapshot(snapshot: &str) -> Result<()> {
    let bytes = snapshot.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err(Error::Config(format!(
            "project.snapshot must be the YYYY-MM-DD date of a Posit Public Package Manager CRAN snapshot, got `{snapshot}`; to use a repository that is identified in any other way, set project.repo-url to its complete URL instead"
        )));
    }
    Ok(())
}

fn validate_repo_url(url: &str) -> Result<()> {
    let invalid = |requirement: &str| {
        Err(Error::Config(format!(
            "project.repo-url {requirement}, got `{url}`"
        )))
    };
    let Some(location) = ["https://", "http://", "file://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
    else {
        return invalid("must be an `https://`, `http://`, or `file://` URL");
    };
    if location.trim_matches('/').is_empty() {
        return invalid("must name a repository");
    }
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return invalid("must not contain whitespace");
    }
    if url.contains(['?', '#']) {
        return invalid(
            "must not contain a query or fragment, because okr appends `src/contrib` paths to it",
        );
    }
    if url.trim_end_matches('/').ends_with("/src/contrib") {
        return invalid("must be the repository root, without the trailing `/src/contrib`");
    }
    Ok(())
}

fn validate_vendor_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(Error::Config(
            "vendor.path must be a normalized project-relative directory".into(),
        ));
    }
    Ok(())
}

fn validate_globs(patterns: &[String], location: &str) -> Result<()> {
    for pattern in patterns {
        GlobBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|error| {
                Error::Config(format!(
                    "{location} contains invalid glob `{pattern}`: {error}"
                ))
            })?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> std::result::Result<(), String> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("`sha256` must be exactly 64 hexadecimal characters".into())
    }
}

fn validate_r_version(version: &str) -> Result<()> {
    let mut components = version.split('.');
    if components.clone().count() == 3
        && components.all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "project.r-version must use R's exact `major.minor.patch` form (for example `4.5.1`), got `{version}`"
        )))
    }
}

fn config_entry_error<T>(kind: EntryKind, name: &str, message: &str) -> Result<T> {
    Err(Error::Config(format!(
        "[{}].{name}: {message}",
        kind.section()
    )))
}

fn contextualize_entry_error(error: Error, kind: EntryKind, name: &str) -> Error {
    Error::Config(format!("[{}].{name}: {error}", kind.section()))
}

#[cfg(test)]
mod tests {
    use super::{Config, DeclaredSource, EntryKind, EntryValue};

    const SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn parses_and_round_trips_the_complete_schema() {
        let input = format!(
            r#"
[project]
r-version = "4.5.1"
snapshot = "2026-06-30"
strict = false

[vendor]
path = "deps-src"
include-tests = true
exclude = ["large/**"]
gitignore = false

[manifest]
agents-file = true

[packages]
rpact = "*"
gsDesign = "3.6.4"
admiral = "pharmaverse/admiral@v1.5.0"
simlib = {{ git = "git@example.test:stats/simlib.git", ref = "v2.1" }}
internalpkg = {{ url = "https://example.test/internalpkg_0.2.1.tar.gz", sha256 = "{SHA256}" }}
rtables = {{ spec = "insightsengineering/rtables@v0.6.13", exclude = ["vignettes/**"], include-tests = false }}

[references]
cdisc-standards = "git::git@example.test:stds/cdisc.git@2026-Q2"
protocol-templates = {{ git = "https://codeberg.org/org/protocols.git", ref = "main" }}
bibtex-guide = {{ url = "https://mirrors.mit.edu/CTAN/info/bibtex/tamethebeast.zip", sha256 = "{SHA256}" }}
"#,
        );

        let parsed = Config::parse(&input).unwrap();
        let serialized = toml::to_string(&parsed).unwrap();
        let reparsed = Config::parse(&serialized).unwrap();
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.declared_entries().unwrap().len(), 9);
        let repository = parsed.repository().unwrap();
        assert_eq!(
            repository.url(),
            "https://packagemanager.posit.co/cran/2026-06-30"
        );
        assert_eq!(repository.snapshot_date(), Some("2026-06-30"));
        assert!(repository.is_fixed());
        assert_eq!(repository.to_string(), "snapshot 2026-06-30");
    }

    #[test]
    fn repo_url_is_the_complete_repository_url_used_as_written() {
        // None of these follows the dated layout of the public snapshots, and
        // none of them is rewritten: only a trailing slash is dropped.
        for (written, root) in [
            (
                "https://ppm.example.test/cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7",
                "https://ppm.example.test/cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7",
            ),
            (
                "https://ppm.example.test/internal/latest",
                "https://ppm.example.test/internal/latest",
            ),
            (
                "https://ppm.example.test/cran/__linux__/noble/2026-06-30",
                "https://ppm.example.test/cran/__linux__/noble/2026-06-30",
            ),
            (
                "https://cloud.r-project.org/",
                "https://cloud.r-project.org",
            ),
            ("http://localhost:8080", "http://localhost:8080"),
            ("file:///srv/cran-mirror", "file:///srv/cran-mirror"),
        ] {
            let parsed = Config::parse(&format!(
                "[project]\nrepo-url = \"{written}\"\n[packages]\npkg = \"*\""
            ))
            .unwrap();
            let repository = parsed.repository().unwrap();
            assert_eq!(repository.url(), root);
            assert_eq!(
                repository.index_url(),
                format!("{root}/src/contrib/PACKAGES.gz")
            );
            assert_eq!(repository.snapshot_date(), None);
            assert!(!repository.is_fixed());
            assert_eq!(repository.to_string(), format!("the repository at {root}"));
            let serialized = toml::to_string(&parsed).unwrap();
            assert_eq!(Config::parse(&serialized).unwrap(), parsed);
        }
    }

    #[test]
    fn snapshot_and_repo_url_are_mutually_exclusive() {
        let error = Config::parse(
            "[project]\nsnapshot = \"2026-06-30\"\nrepo-url = \"https://ppm.example.test/cran/\"",
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        let message = error.to_string();
        assert!(
            message.contains("project.snapshot and project.repo-url cannot both be set"),
            "{message}"
        );
        // The message names the URL that reads the same repository as the
        // two keys once did together.
        assert!(
            message.contains("`repo-url = \"https://ppm.example.test/cran/2026-06-30\"`"),
            "{message}"
        );
    }

    #[test]
    fn snapshot_is_a_date_and_other_identifiers_are_sent_to_repo_url() {
        for snapshot in ["latest", "2026-6-30", "2026-06-30+Y3JhbiwyOjQ1MjYyMTU7", ""] {
            let error =
                Config::parse(&format!("[project]\nsnapshot = \"{snapshot}\"")).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("YYYY-MM-DD"), "{message}");
            assert!(message.contains("project.repo-url"), "{message}");
        }
    }

    #[test]
    fn repo_url_must_be_a_repository_root_url() {
        for (url, requirement) in [
            ("", "`https://`, `http://`, or `file://` URL"),
            (
                "ppm.example.test/cran",
                "`https://`, `http://`, or `file://` URL",
            ),
            (
                "ftp://example.test/cran",
                "`https://`, `http://`, or `file://` URL",
            ),
            ("https://", "must name a repository"),
            ("file:///", "must name a repository"),
            (
                "https://example.test/my cran",
                "must not contain whitespace",
            ),
            ("https://example.test/cran?token=1", "query or fragment"),
            ("https://example.test/cran#latest", "query or fragment"),
            (
                "https://example.test/cran/src/contrib/",
                "without the trailing `/src/contrib`",
            ),
        ] {
            let error = Config::parse(&format!("[project]\nrepo-url = \"{url}\"")).unwrap_err();
            let message = error.to_string();
            assert_eq!(error.exit_code(), 2, "{message}");
            assert!(message.contains("project.repo-url"), "{message}");
            assert!(message.contains(requirement), "{message}");
            assert!(message.contains(&format!("got `{url}`")), "{message}");
        }
    }

    #[test]
    fn defaults_match_the_documented_pit_of_success() {
        let parsed = Config::parse("").unwrap();
        assert_eq!(parsed.vendor.path.to_string_lossy(), "deps-src");
        assert!(parsed.vendor.include_tests);
        assert!(parsed.vendor.gitignore);
        assert!(!parsed.manifest.agents_file);
        assert!(!parsed.project.strict);
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        for input in [
            "typo = true",
            "[project]\ntyop = true",
            "[project]\nname = \"unused-label\"",
            "[vendor]\ninclude-test = true",
            "[manifest]\nagent-file = true",
        ] {
            let error = Config::parse(input).unwrap_err();
            assert!(
                error.to_string().contains("unknown field"),
                "unexpected error for `{input}`: {error}"
            );
        }

        // Serde's untagged-enum diagnostic folds the inner unknown-field error
        // into "did not match any variant", but these remain hard failures.
        for input in [
            "[packages]\npkg = { git = \"host:path\", typo = true }",
            "[references]\nref = { git = \"host:path\", typo = true }",
        ] {
            assert!(
                Config::parse(input).is_err(),
                "`{input}` unexpectedly parsed"
            );
        }
    }

    #[test]
    fn r_version_is_an_exact_runtime_version() {
        for version in ["4", "4.5", "4.5.1 Patched", "latest", ""] {
            let error =
                Config::parse(&format!("[project]\nr-version = \"{version}\"")).unwrap_err();
            assert!(error.to_string().contains("major.minor.patch"), "{error}");
        }

        let parsed = Config::parse("[project]\nr-version = \"4.5.1\"").unwrap();
        assert_eq!(parsed.project.r_version.as_deref(), Some("4.5.1"));
    }

    #[test]
    fn url_sources_require_a_valid_declared_digest() {
        for input in [
            "[packages]\npkg = \"url::https://example.test/pkg.tar.gz\"",
            "[packages]\npkg = { url = \"https://example.test/pkg.tar.gz\" }",
            "[references]\nref = { url = \"https://example.test/ref.tgz\", sha256 = \"short\" }",
            "[references]\nnotes = { url = \"https://example.test/notes.zip\" }",
        ] {
            let error = Config::parse(input).unwrap_err();
            assert!(error.to_string().contains("sha256"), "{error}");
        }
    }

    #[test]
    fn url_sources_must_name_a_supported_archive() {
        let error = Config::parse(&format!(
            "[references]\nnotes = {{ url = \"https://example.test/notes.7z\", sha256 = \"{SHA256}\" }}"
        ))
        .unwrap_err();
        assert!(error.to_string().contains("[references].notes"), "{error}");
        assert!(
            error.to_string().contains(".tar.gz, .tgz, or .zip"),
            "{error}"
        );

        let parsed = Config::parse(&format!(
            "[references]\nnotes = {{ url = \"https://example.test/notes.zip\", sha256 = \"{SHA256}\" }}"
        ))
        .unwrap();
        let entry = parsed.declared_entries().unwrap().remove(0);
        assert_eq!(entry.kind, EntryKind::Reference);
        assert!(matches!(
            entry.source,
            DeclaredSource::Remote {
                expected_sha256: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn cran_specs_are_forbidden_in_references() {
        for input in [
            "[references]\nthing = \"*\"",
            "[references]\nthing = \"1.2.3\"",
            "[references]\nthing = { spec = \"1.2.3\" }",
        ] {
            let error = Config::parse(input).unwrap_err();
            assert!(error.to_string().contains("CRAN"), "{error}");
            assert!(error.to_string().contains("[references].thing"), "{error}");
        }
    }

    #[test]
    fn cran_requires_a_repository_but_remote_only_configs_do_not() {
        let error = Config::parse("[packages]\npkg = \"*\"").unwrap_err();
        assert!(error.to_string().contains("project.snapshot"), "{error}");
        assert!(error.to_string().contains("project.repo-url"), "{error}");

        // Either key supplies the repository.
        Config::parse("[project]\nsnapshot = \"2026-06-30\"\n[packages]\npkg = \"*\"").unwrap();
        Config::parse(
            "[project]\nrepo-url = \"https://ppm.example.test/cran/latest\"\n[packages]\npkg = \"*\"",
        )
        .unwrap();

        let remote_only = Config::parse("[packages]\npkg = \"owner/repo@v1\"").unwrap();
        assert_eq!(remote_only.repository(), None);
    }

    #[test]
    fn table_source_selection_and_ref_are_validated() {
        for input in [
            "[packages]\npkg = {}",
            "[packages]\npkg = { git = \"host:path\", spec = \"owner/repo\" }",
            "[packages]\npkg = { spec = \"owner/repo@main\", ref = \"other\" }",
            "[packages]\npkg = { spec = \"*\", ref = \"main\" }",
        ] {
            assert!(
                Config::parse(input).is_err(),
                "`{input}` unexpectedly parsed"
            );
        }
    }

    #[test]
    fn declaration_preserves_per_entry_options() {
        let input = r#"
[packages]
pkg = { spec = "owner/repo", exclude = ["docs/**"], include-tests = false }
"#;
        let config = Config::parse(input).unwrap();
        let entry = config.declared_entries().unwrap().remove(0);
        assert_eq!(entry.kind, EntryKind::Package);
        assert_eq!(entry.exclude, ["docs/**"]);
        assert_eq!(entry.include_tests, Some(false));
        assert!(matches!(entry.source, DeclaredSource::Remote { .. }));
    }

    #[test]
    fn entry_values_deserialize_as_strings_or_tables() {
        let config = Config::parse(
            "[project]\nsnapshot = \"2026-06-30\"\n[packages]\na = \"*\"\nb = { spec = \"org/b\" }",
        )
        .unwrap();
        assert!(matches!(config.packages["a"], EntryValue::String(_)));
        assert!(matches!(config.packages["b"], EntryValue::Table(_)));
    }

    #[test]
    fn vendor_paths_and_exclude_globs_are_safe() {
        for input in [
            "[vendor]\npath = \"../outside\"",
            "[vendor]\npath = \"/absolute\"",
            "[vendor]\nexclude = [\"[invalid\"]",
            "[packages]\npkg = { spec = \"org/repo\", exclude = [\"[invalid\"] }",
        ] {
            assert!(
                Config::parse(input).is_err(),
                "`{input}` unexpectedly parsed"
            );
        }
    }

    #[test]
    fn package_and_reference_directory_names_cannot_collide() {
        let error = Config::parse(
            "[packages]\nshared = \"org/shared\"\n[references]\nshared = \"org/other\"",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("both [packages] and [references]")
        );
    }
}
