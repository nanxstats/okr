//! Strict project configuration models and validation.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};

use crate::spec::{PackageSpec, RemoteLocation, RemoteSpec, parse_package};
use crate::{Error, Result};

pub const DEFAULT_REPO_URL: &str = "https://packagemanager.posit.co/cran";

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

        if has_cran && self.project.snapshot.is_none() {
            return Err(Error::Config(
                "project.snapshot is required when [packages] contains a CRAN entry".into(),
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

    #[must_use]
    pub fn repository_url(&self) -> &str {
        self.project
            .repo_url
            .as_deref()
            .unwrap_or(DEFAULT_REPO_URL)
            .trim_end_matches('/')
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub r_version: Option<String>,
    pub snapshot: Option<String>,
    pub strict: bool,
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
            "project.snapshot must use YYYY-MM-DD format, got `{snapshot}`"
        )));
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
repo-url = "https://example.test/cran"

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
"#,
        );

        let parsed = Config::parse(&input).unwrap();
        let serialized = toml::to_string(&parsed).unwrap();
        let reparsed = Config::parse(&serialized).unwrap();
        assert_eq!(parsed, reparsed);
        assert_eq!(parsed.declared_entries().unwrap().len(), 8);
        assert_eq!(parsed.repository_url(), "https://example.test/cran");
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
        ] {
            let error = Config::parse(input).unwrap_err();
            assert!(error.to_string().contains("sha256"), "{error}");
        }
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
    fn cran_requires_a_snapshot_but_remote_only_configs_do_not() {
        let error = Config::parse("[packages]\npkg = \"*\"").unwrap_err();
        assert!(error.to_string().contains("project.snapshot"));

        Config::parse("[packages]\npkg = \"owner/repo@v1\"").unwrap();
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
