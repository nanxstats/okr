//! Stable lockfile models, construction, and atomic serialization.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::config::{Config, EntryKind};
use crate::digest::sha256_bytes;
use crate::resolve::{Resolution, ResolvedEntry, ResolvedSource};
use crate::vendor::{VendorResult, VendoredEntry};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Lockfile {
    pub version: u32,
    pub okr_version: String,
    pub generated: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub config_hash: String,
    pub environment_digest: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
    #[serde(default, rename = "reference")]
    pub references: Vec<LockedReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fetch_method: FetchMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    pub tree_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LockedReference {
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fetch_method: FetchMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    pub tree_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FetchMethod {
    Tarball,
    ForgeTarball,
    Gh,
    GitClone,
}

impl Lockfile {
    pub fn build(
        config: &Config,
        resolution: &Resolution,
        vendored: &VendorResult,
    ) -> Result<Self> {
        let mut packages = Vec::new();
        let mut references = Vec::new();
        for resolved in &resolution.entries {
            let tree = vendored
                .entries
                .iter()
                .find(|entry| entry.kind == resolved.kind && entry.name == resolved.name)
                .ok_or_else(|| {
                    Error::Io(std::io::Error::other(format!(
                        "vendoring produced no metadata for {}",
                        resolved.name
                    )))
                })?;
            match resolved.kind {
                EntryKind::Package => packages.push(lock_package(resolved, tree)?),
                EntryKind::Reference => references.push(lock_reference(resolved, tree)),
            }
        }
        packages.sort_by(|left, right| left.name.cmp(&right.name));
        references.sort_by(|left, right| left.name.cmp(&right.name));
        let snapshot = resolution
            .entries
            .iter()
            .any(|entry| matches!(entry.source, ResolvedSource::Cran { .. }))
            .then(|| config.project.snapshot.clone())
            .flatten();
        let mut lock = Self {
            version: 1,
            okr_version: env!("CARGO_PKG_VERSION").into(),
            generated: deterministic_generated(snapshot.as_deref()),
            snapshot,
            config_hash: config_hash(config)?,
            environment_digest: String::new(),
            packages,
            references,
        };
        lock.environment_digest = lock.computed_environment_digest()?;
        Ok(lock)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path).map_err(|error| {
            Error::Config(format!(
                "could not read lockfile {}: {error}",
                path.display()
            ))
        })?;
        toml::from_str(&contents)
            .map_err(|error| Error::Config(format!("invalid {}: {error}", path.display())))
    }

    pub fn load_optional(path: &Path) -> Result<Option<Self>> {
        if path.try_exists()? {
            Self::load(path).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn to_stable_toml(&self) -> Result<String> {
        let mut stable = self.clone();
        stable
            .packages
            .sort_by(|left, right| left.name.cmp(&right.name));
        stable
            .references
            .sort_by(|left, right| left.name.cmp(&right.name));
        stable.environment_digest = stable.computed_environment_digest()?;
        let mut output = toml::to_string_pretty(&stable)
            .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        if !output.ends_with('\n') {
            output.push('\n');
        }
        Ok(output)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(self.to_stable_toml()?.as_bytes())?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|error| Error::Io(error.error))?;
        Ok(())
    }

    pub fn computed_environment_digest(&self) -> Result<String> {
        let mut packages = self.packages.clone();
        let mut references = self.references.clone();
        packages.sort_by(|left, right| left.name.cmp(&right.name));
        references.sort_by(|left, right| left.name.cmp(&right.name));
        #[derive(Serialize)]
        struct Environment<'a> {
            version: u32,
            package: &'a [LockedPackage],
            reference: &'a [LockedReference],
        }
        let canonical = serde_json::to_vec(&Environment {
            version: self.version,
            package: &packages,
            reference: &references,
        })
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        Ok(format!("sha256:{}", sha256_bytes(canonical)))
    }

    #[must_use]
    pub fn is_sorted(&self) -> bool {
        self.packages
            .windows(2)
            .all(|pair| pair[0].name <= pair[1].name)
            && self
                .references
                .windows(2)
                .all(|pair| pair[0].name <= pair[1].name)
    }
}

pub fn config_hash(config: &Config) -> Result<String> {
    let normalized = toml::to_string(config)
        .map_err(|error| Error::Config(format!("could not normalize okr.toml: {error}")))?;
    Ok(format!("sha256:{}", sha256_bytes(normalized)))
}

fn lock_package(resolved: &ResolvedEntry, tree: &VendoredEntry) -> Result<LockedPackage> {
    let version = tree.version.clone().ok_or_else(|| {
        Error::Fetch(format!(
            "vendored package {} has no DESCRIPTION version",
            resolved.name
        ))
    })?;
    let (source, url, reference, commit) = source_fields(&resolved.source);
    Ok(LockedPackage {
        name: resolved.name.clone(),
        version,
        source,
        url,
        reference,
        commit,
        fetch_method: tree.fetch_method,
        tarball_sha256: Some(tree.artifact_sha256.clone()),
        tree_digest: tree.tree.digest.clone(),
        license: tree.license.clone(),
    })
}

fn lock_reference(resolved: &ResolvedEntry, tree: &VendoredEntry) -> LockedReference {
    let (source, url, reference, commit) = source_fields(&resolved.source);
    LockedReference {
        name: resolved.name.clone(),
        source,
        url,
        reference,
        commit,
        fetch_method: tree.fetch_method,
        tarball_sha256: Some(tree.artifact_sha256.clone()),
        tree_digest: tree.tree.digest.clone(),
        license: tree.license.clone(),
    }
}

fn source_fields(
    source: &ResolvedSource,
) -> (String, Option<String>, Option<String>, Option<String>) {
    match source {
        ResolvedSource::Cran { url, .. } => ("cran".into(), Some(url.clone()), None, None),
        ResolvedSource::Tarball {
            source,
            url,
            reference,
        } => (source.clone(), Some(url.clone()), reference.clone(), None),
        ResolvedSource::Git {
            source,
            locked_ref,
            commit,
            ..
        } => (
            source.clone(),
            None,
            locked_ref.clone(),
            Some(commit.clone()),
        ),
    }
}

fn deterministic_generated(snapshot: Option<&str>) -> String {
    snapshot.map_or_else(
        || "1970-01-01T00:00:00Z".into(),
        |date| format!("{date}T00:00:00Z"),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationReport {
    pub schema: u32,
    pub ok: bool,
    pub environment_digest: String,
    pub mismatches: Vec<FileMismatch>,
}

impl VerificationReport {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.ok
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct FileMismatch {
    pub entry_kind: String,
    pub entry: String,
    pub path: String,
    pub mismatch: MismatchKind,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MismatchKind {
    Missing,
    Modified,
    Unexpected,
    Unreadable,
    LockDigest,
    LockSchema,
    LockOrder,
}

/// Recompute and verify each aggregate tree digest under the vendor root.
pub fn verify_vendor(
    project_directory: &Path,
    config: &Config,
    lock: &Lockfile,
) -> VerificationReport {
    let mut mismatches = Vec::new();
    if lock.version != 1 {
        mismatches.push(lock_mismatch(
            "version",
            MismatchKind::LockSchema,
            Some("1".into()),
            Some(lock.version.to_string()),
        ));
    }
    if !lock.is_sorted() {
        mismatches.push(lock_mismatch(
            "entry-order",
            MismatchKind::LockOrder,
            Some("sorted by kind and name".into()),
            Some("unsorted".into()),
        ));
    }
    match lock.computed_environment_digest() {
        Ok(actual) if actual != lock.environment_digest => mismatches.push(lock_mismatch(
            "environment-digest",
            MismatchKind::LockDigest,
            Some(lock.environment_digest.clone()),
            Some(actual),
        )),
        Err(error) => mismatches.push(lock_mismatch(
            "environment-digest",
            MismatchKind::Unreadable,
            Some(lock.environment_digest.clone()),
            Some(error.to_string()),
        )),
        Ok(_) => {}
    }

    let root = project_directory.join(&config.vendor.path);
    let root_exists = root.try_exists().unwrap_or(false);
    for package in &lock.packages {
        verify_entry(
            &root,
            root_exists,
            "package",
            &package.name,
            &package.tree_digest,
            &mut mismatches,
        );
    }
    for reference in &lock.references {
        verify_entry(
            &root,
            root_exists,
            "reference",
            &reference.name,
            &reference.tree_digest,
            &mut mismatches,
        );
    }
    verify_root_entries(&root, root_exists, lock, &mut mismatches);
    verify_manifests(&root, root_exists, config, lock, &mut mismatches);
    mismatches.sort_by(|left, right| {
        (
            &left.entry_kind,
            &left.entry,
            &left.path,
            mismatch_name(left.mismatch),
        )
            .cmp(&(
                &right.entry_kind,
                &right.entry,
                &right.path,
                mismatch_name(right.mismatch),
            ))
    });
    VerificationReport {
        schema: 1,
        ok: mismatches.is_empty(),
        environment_digest: lock.environment_digest.clone(),
        mismatches,
    }
}

fn verify_entry(
    root: &Path,
    root_exists: bool,
    entry_kind: &str,
    name: &str,
    expected_tree_digest: &str,
    mismatches: &mut Vec<FileMismatch>,
) {
    let directory = root.join(name);
    if !root_exists || !directory.is_dir() {
        mismatches.push(file_mismatch(
            entry_kind,
            name,
            ".",
            MismatchKind::Missing,
            Some(expected_tree_digest.to_owned()),
            None,
        ));
        return;
    }

    let actual = match crate::digest::tree_digest(&directory) {
        Ok(actual) => actual,
        Err(error) => {
            mismatches.push(file_mismatch(
                entry_kind,
                name,
                ".",
                MismatchKind::Unreadable,
                Some(expected_tree_digest.to_owned()),
                Some(error.to_string()),
            ));
            return;
        }
    };
    if actual.digest != expected_tree_digest {
        mismatches.push(file_mismatch(
            entry_kind,
            name,
            ".",
            MismatchKind::Modified,
            Some(expected_tree_digest.to_owned()),
            Some(actual.digest),
        ));
    }
}

fn verify_root_entries(
    root: &Path,
    root_exists: bool,
    lock: &Lockfile,
    mismatches: &mut Vec<FileMismatch>,
) {
    if !root_exists {
        if lock.packages.is_empty() && lock.references.is_empty() {
            mismatches.push(file_mismatch(
                "vendor",
                "deps-src",
                ".",
                MismatchKind::Missing,
                Some("vendor directory".into()),
                None,
            ));
        }
        return;
    }
    let expected = lock
        .packages
        .iter()
        .map(|entry| entry.name.as_str())
        .chain(lock.references.iter().map(|entry| entry.name.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    let Ok(entries) = fs::read_dir(root) else {
        mismatches.push(file_mismatch(
            "vendor",
            "deps-src",
            ".",
            MismatchKind::Unreadable,
            Some("readable vendor directory".into()),
            None,
        ));
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), "_manifest.json" | "_manifest.md")
            || expected.contains(name.as_str())
        {
            continue;
        }
        mismatches.push(file_mismatch(
            "vendor",
            "deps-src",
            &name,
            MismatchKind::Unexpected,
            None,
            Some("unlocked root entry".into()),
        ));
    }
}

fn verify_manifests(
    root: &Path,
    root_exists: bool,
    config: &Config,
    lock: &Lockfile,
    mismatches: &mut Vec<FileMismatch>,
) {
    if !root_exists {
        return;
    }
    let rendered = match crate::manifest::render_for_verification(config, lock, root) {
        Ok(rendered) => rendered,
        Err(error) => {
            mismatches.push(file_mismatch(
                "manifest",
                "deps-src",
                ".",
                MismatchKind::Unreadable,
                Some("manifests reconstructable from locked metadata".into()),
                Some(error.to_string()),
            ));
            return;
        }
    };
    for (name, expected) in [
        ("_manifest.json", rendered.json),
        ("_manifest.md", rendered.markdown),
    ] {
        let path = root.join(name);
        match fs::read_to_string(path) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => mismatches.push(file_mismatch(
                "manifest",
                "deps-src",
                name,
                MismatchKind::Modified,
                Some(format!("sha256:{}", sha256_bytes(expected))),
                Some(format!("sha256:{}", sha256_bytes(actual))),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                mismatches.push(file_mismatch(
                    "manifest",
                    "deps-src",
                    name,
                    MismatchKind::Missing,
                    Some(format!("sha256:{}", sha256_bytes(expected))),
                    None,
                ));
            }
            Err(error) => mismatches.push(file_mismatch(
                "manifest",
                "deps-src",
                name,
                MismatchKind::Unreadable,
                Some(format!("sha256:{}", sha256_bytes(expected))),
                Some(error.to_string()),
            )),
        }
    }
}

fn file_mismatch(
    entry_kind: &str,
    entry: &str,
    path: &str,
    mismatch: MismatchKind,
    expected: Option<String>,
    actual: Option<String>,
) -> FileMismatch {
    FileMismatch {
        entry_kind: entry_kind.into(),
        entry: entry.into(),
        path: path.into(),
        mismatch,
        expected,
        actual,
    }
}

fn lock_mismatch(
    path: &str,
    mismatch: MismatchKind,
    expected: Option<String>,
    actual: Option<String>,
) -> FileMismatch {
    file_mismatch("lock", "okr.lock", path, mismatch, expected, actual)
}

const fn mismatch_name(kind: MismatchKind) -> &'static str {
    match kind {
        MismatchKind::Missing => "missing",
        MismatchKind::Modified => "modified",
        MismatchKind::Unexpected => "unexpected",
        MismatchKind::Unreadable => "unreadable",
        MismatchKind::LockDigest => "lock-digest",
        MismatchKind::LockSchema => "lock-schema",
        MismatchKind::LockOrder => "lock-order",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{FetchMethod, LockedPackage, LockedReference, Lockfile, config_hash};
    use crate::config::Config;

    fn example_lock() -> Lockfile {
        Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "2026-06-30T00:00:00Z".into(),
            snapshot: Some("2026-06-30".into()),
            config_hash: format!("sha256:{}", "a".repeat(64)),
            environment_digest: format!("sha256:{}", "b".repeat(64)),
            packages: vec![LockedPackage {
                name: "rpact".into(),
                version: "4.2.1".into(),
                source: "cran".into(),
                url: Some("https://example.test/rpact_4.2.1.tar.gz".into()),
                reference: None,
                commit: None,
                fetch_method: FetchMethod::Tarball,
                tarball_sha256: Some("c".repeat(64)),
                tree_digest: format!("sha256:{}", "d".repeat(64)),
                license: Some("LGPL-2.1".into()),
            }],
            references: vec![LockedReference {
                name: "standards".into(),
                source: "git::git@example.test:stds/standards.git".into(),
                url: None,
                reference: Some("main".into()),
                commit: Some("e".repeat(40)),
                fetch_method: FetchMethod::GitClone,
                tarball_sha256: Some("f".repeat(64)),
                tree_digest: format!("sha256:{}", "0".repeat(64)),
                license: None,
            }],
        }
    }

    #[test]
    fn lock_model_round_trips_the_current_schema() {
        let lock = example_lock();
        let encoded = toml::to_string(&lock).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, lock);
        assert!(encoded.contains("fetch-method = \"git-clone\""));
        assert!(encoded.contains("[[package]]"));
        assert!(encoded.contains("[[reference]]"));
    }

    #[test]
    fn lockfile_serialization_has_an_insta_snapshot() {
        let mut lock = example_lock();
        lock.environment_digest = lock.computed_environment_digest().unwrap();
        let encoded = lock.to_stable_toml().unwrap();
        insta::assert_snapshot!(encoded);
    }

    #[test]
    fn write_is_atomic_and_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("okr.lock");
        let mut lock = example_lock();
        lock.environment_digest = lock.computed_environment_digest().unwrap();
        lock.write(&path).unwrap();
        assert_eq!(Lockfile::load(&path).unwrap(), lock);
    }

    #[test]
    fn config_hash_ignores_toml_comments_and_formatting() {
        let first = Config::parse(
            "# heading\n[project]\nstrict = false\n[packages]\npkg = \"owner/repo@main\"\n",
        )
        .unwrap();
        let second = Config::parse(
            "[project]\nstrict=false\n\n[packages]\n# package\npkg=\"owner/repo@main\"",
        )
        .unwrap();
        assert_eq!(config_hash(&first).unwrap(), config_hash(&second).unwrap());
    }

    #[test]
    fn environment_digest_is_order_independent_but_content_sensitive() {
        let mut lock = example_lock();
        let first = lock.computed_environment_digest().unwrap();
        lock.packages.reverse();
        lock.references.reverse();
        assert_eq!(lock.computed_environment_digest().unwrap(), first);
        lock.packages[0].tree_digest = format!("sha256:{}", "9".repeat(64));
        assert_ne!(lock.computed_environment_digest().unwrap(), first);
    }

    #[test]
    fn verify_reports_an_aggregate_tree_mismatch_as_json() {
        let directory = tempdir().unwrap();
        let package = directory.path().join("deps-src/pkg");
        fs::create_dir_all(package.join("R")).unwrap();
        fs::write(
            package.join("DESCRIPTION"),
            b"Package: pkg\nVersion: 1.0.0\n",
        )
        .unwrap();
        fs::write(package.join("R/code.R"), b"value <- 1\n").unwrap();
        let tree = crate::digest::tree_digest(&package).unwrap();
        let mut lock = Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "1970-01-01T00:00:00Z".into(),
            snapshot: None,
            config_hash: format!("sha256:{}", "1".repeat(64)),
            environment_digest: String::new(),
            packages: vec![LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                source: "github::owner/pkg".into(),
                url: None,
                reference: Some("v1".into()),
                commit: Some("2".repeat(40)),
                fetch_method: FetchMethod::ForgeTarball,
                tarball_sha256: Some("3".repeat(64)),
                tree_digest: tree.digest,
                license: Some("MIT".into()),
            }],
            references: Vec::new(),
        };
        lock.environment_digest = lock.computed_environment_digest().unwrap();
        let config = Config::default();
        let rendered = crate::manifest::render_for_verification(
            &config,
            &lock,
            &directory.path().join("deps-src"),
        )
        .unwrap();
        fs::write(
            directory.path().join("deps-src/_manifest.json"),
            rendered.json,
        )
        .unwrap();
        fs::write(
            directory.path().join("deps-src/_manifest.md"),
            rendered.markdown,
        )
        .unwrap();
        assert!(super::verify_vendor(directory.path(), &config, &lock).is_clean());

        fs::write(package.join("R/code.R"), b"value <- 2\n").unwrap();
        let report = super::verify_vendor(directory.path(), &config, &lock);
        assert!(!report.is_clean());
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].path, ".");
        assert_eq!(report.mismatches[0].mismatch, super::MismatchKind::Modified);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(&report.mismatches[0].expected.clone().unwrap()));
        assert!(json.contains(&report.mismatches[0].actual.clone().unwrap()));
        assert!(json.contains("modified"));

        fs::write(package.join("R/code.R"), b"value <- 1\n").unwrap();
        fs::write(
            directory.path().join("deps-src/_manifest.md"),
            b"tampered manifest\n",
        )
        .unwrap();
        let report = super::verify_vendor(directory.path(), &config, &lock);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].path, "_manifest.md");
    }

    #[test]
    fn verify_detects_tree_drift_and_unlocked_roots() {
        let directory = tempdir().unwrap();
        let package = directory.path().join("deps-src/pkg");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("expected"), b"expected").unwrap();
        let tree = crate::digest::tree_digest(&package).unwrap();
        let mut lock = example_lock();
        lock.packages.truncate(1);
        lock.packages[0].name = "pkg".into();
        lock.packages[0].tree_digest = tree.digest;
        lock.references.clear();
        lock.environment_digest = lock.computed_environment_digest().unwrap();
        fs::remove_file(package.join("expected")).unwrap();
        fs::write(package.join("unexpected"), b"unexpected").unwrap();
        fs::create_dir_all(directory.path().join("deps-src/unlocked")).unwrap();
        let report = super::verify_vendor(directory.path(), &Config::default(), &lock);
        assert!(report.mismatches.iter().any(|item| {
            item.entry == "pkg"
                && item.path == "."
                && item.mismatch == super::MismatchKind::Modified
        }));
        assert!(
            report
                .mismatches
                .iter()
                .any(|item| { item.path == "unlocked" && item.entry_kind == "vendor" })
        );
    }

    #[test]
    fn lock_unknown_keys_are_rejected() {
        let encoded = toml::to_string(&example_lock()).unwrap();
        let malformed = encoded.replacen("version = 1", "version = 1\ntyop = true", 1);
        let error = toml::from_str::<Lockfile>(&malformed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
