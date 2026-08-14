//! Deterministic source extraction, pruning, and replacement.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use tempfile::{Builder as TempBuilder, TempDir};
use walkdir::WalkDir;

use crate::config::{Config, EntryKind};
use crate::digest::{TreeDigest, tree_digest};
use crate::fetch::{CachedArtifact, Fetcher};
use crate::hosttools::HostTools;
use crate::lock::FetchMethod;
use crate::progress::SyncProgress;
use crate::resolve::dcf;
use crate::resolve::{GithubRepository, Resolution, ResolvedEntry, ResolvedSource};
use crate::{Error, Result};

const PACKAGE_EXCLUDES: &[&str] = &[
    "data/**",
    "pkgdown/**",
    "docs/**",
    ".github/**",
    ".git*",
    "revdep/**",
    "**/*.rda",
    "**/*.rds",
    "**/*.RData",
];
const REFERENCE_EXCLUDES: &[&str] = &[".git*"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorResult {
    pub root: PathBuf,
    pub entries: Vec<VendoredEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredEntry {
    pub name: String,
    pub kind: EntryKind,
    pub version: Option<String>,
    pub license: Option<String>,
    pub title: Option<String>,
    pub fetch_method: FetchMethod,
    pub artifact_sha256: String,
    pub tree: TreeDigest,
}

pub fn vendor(
    project_directory: &Path,
    config: &Config,
    resolution: &Resolution,
    fetcher: &Fetcher,
    tools: &HostTools,
) -> Result<VendorResult> {
    vendor_inner(project_directory, config, resolution, fetcher, tools, None)
}

pub(crate) fn vendor_with_progress(
    project_directory: &Path,
    config: &Config,
    resolution: &Resolution,
    fetcher: &Fetcher,
    tools: &HostTools,
    progress: &SyncProgress,
) -> Result<VendorResult> {
    vendor_inner(
        project_directory,
        config,
        resolution,
        fetcher,
        tools,
        Some(progress),
    )
}

fn vendor_inner(
    project_directory: &Path,
    config: &Config,
    resolution: &Resolution,
    fetcher: &Fetcher,
    tools: &HostTools,
    progress: Option<&SyncProgress>,
) -> Result<VendorResult> {
    let target_root = project_directory.join(&config.vendor.path);
    let parent = target_root.parent().ok_or_else(|| {
        Error::Config(format!(
            "vendor path has no parent: {}",
            target_root.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let staging = TempBuilder::new()
        .prefix(".okr-vendor-")
        .tempdir_in(parent)?;
    let mut entries = Vec::with_capacity(resolution.entries.len());
    let mut warnings = resolution.warnings.clone();

    if let Some(progress) = progress {
        progress.set_phase("Fetching and vendoring sources...");
    }
    for entry in &resolution.entries {
        let entry_progress = progress.map(|progress| progress.entry("Preparing", &entry.name));
        let destination = staging.path().join(&entry.name);
        let vendored = vendor_entry(
            parent,
            &destination,
            config,
            entry,
            fetcher,
            tools,
            &mut warnings,
        )?;
        if let Some(entry_progress) = entry_progress {
            entry_progress.finish();
        }
        if let Some(progress) = progress {
            progress.advance(format!("Prepared {}", entry.name));
        }
        entries.push(vendored);
    }

    replace_directory(staging, &target_root)?;
    Ok(VendorResult {
        root: target_root,
        entries,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
fn vendor_entry(
    temporary_parent: &Path,
    destination: &Path,
    config: &Config,
    entry: &ResolvedEntry,
    fetcher: &Fetcher,
    tools: &HostTools,
    warnings: &mut Vec<String>,
) -> Result<VendoredEntry> {
    let acquisition = acquire(temporary_parent, entry, fetcher, tools, warnings)?;
    let (raw, tarball) = match acquisition {
        Acquisition::Tarball { artifact, method } => {
            let raw = TempBuilder::new()
                .prefix(".okr-extract-")
                .tempdir_in(temporary_parent)?;
            extract_tarball(&artifact.path, raw.path()).map_err(|error| {
                Error::Fetch(format!(
                    "could not extract source for {} from {}: {error}",
                    entry.name,
                    artifact.path.display()
                ))
            })?;
            (raw, Some((artifact, method)))
        }
        Acquisition::Clone { directory } => (directory, None),
    };

    let include_tests = entry.include_tests.unwrap_or(config.vendor.include_tests);
    let mut user_excludes = config.vendor.exclude.clone();
    user_excludes.extend(entry.exclude.iter().cloned());
    let excludes = build_excludes(entry.kind, include_tests, &user_excludes)?;
    copy_pruned(raw.path(), destination, &excludes)?;

    let (version, license, title) = metadata(entry, destination)?;
    let tree = tree_digest(destination)?;
    let (fetch_method, artifact_sha256) = match tarball {
        Some((artifact, method)) => (method, artifact.sha256),
        None => {
            let key = clone_cache_key(entry);
            let artifact = fetcher.cache().put_normalized_tree(&key, destination)?;
            if let Some(expected) = &entry.artifact_sha256
                && artifact.sha256 != *expected
            {
                return Err(Error::Fetch(format!(
                    "normalized clone artifact for {} changed: expected {expected}, found {}",
                    entry.name, artifact.sha256
                )));
            }
            (FetchMethod::GitClone, artifact.sha256)
        }
    };

    Ok(VendoredEntry {
        name: entry.name.clone(),
        kind: entry.kind,
        version,
        license,
        title,
        fetch_method,
        artifact_sha256,
        tree,
    })
}

enum Acquisition {
    Tarball {
        artifact: CachedArtifact,
        method: FetchMethod,
    },
    Clone {
        directory: TempDir,
    },
}

fn acquire(
    temporary_parent: &Path,
    entry: &ResolvedEntry,
    fetcher: &Fetcher,
    tools: &HostTools,
    warnings: &mut Vec<String>,
) -> Result<Acquisition> {
    match &entry.source {
        ResolvedSource::Cran { url, .. } | ResolvedSource::Tarball { url, .. } => {
            let artifact = fetcher.fetch_url(
                url,
                entry.artifact_sha256.as_deref(),
                &format!("source for {}", entry.name),
            )?;
            Ok(Acquisition::Tarball {
                artifact,
                method: FetchMethod::Tarball,
            })
        }
        ResolvedSource::Git {
            clone_url,
            locked_ref,
            commit,
            archive_url,
            github,
            ..
        } => acquire_git(
            temporary_parent,
            entry,
            fetcher,
            tools,
            warnings,
            clone_url,
            locked_ref.as_deref(),
            commit,
            archive_url.as_deref(),
            github.as_ref(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn acquire_git(
    temporary_parent: &Path,
    entry: &ResolvedEntry,
    fetcher: &Fetcher,
    tools: &HostTools,
    warnings: &mut Vec<String>,
    clone_url: &str,
    locked_ref: Option<&str>,
    commit: &str,
    archive_url: Option<&str>,
    github: Option<&GithubRepository>,
) -> Result<Acquisition> {
    if let (Some(method), Some(digest)) = (
        entry.preferred_fetch_method,
        entry.artifact_sha256.as_deref(),
    ) {
        if let Some(artifact) = fetcher.cache().get(digest)? {
            return Ok(Acquisition::Tarball { artifact, method });
        }
        if fetcher.is_offline() {
            return Err(Error::Fetch(format!(
                "offline mode: missing cached artifact {digest} for {}",
                entry.name
            )));
        }
        match method {
            FetchMethod::ForgeTarball => {
                let url = archive_url.ok_or_else(|| {
                    Error::Fetch(format!(
                        "cannot replay forge-tarball fetch for {}: no forge archive URL",
                        entry.name
                    ))
                })?;
                let artifact = fetcher.fetch_url(url, Some(digest), &entry.name)?;
                return Ok(Acquisition::Tarball { artifact, method });
            }
            FetchMethod::Gh => {
                let github = github.ok_or_else(|| {
                    Error::Fetch(format!(
                        "cannot replay gh fetch for {}: source is not GitHub",
                        entry.name
                    ))
                })?;
                let artifact =
                    acquire_github_api_tarball(fetcher, tools, github, commit, Some(digest))?;
                return Ok(Acquisition::Tarball { artifact, method });
            }
            FetchMethod::GitClone => {
                return clone_source(temporary_parent, tools, clone_url, locked_ref, commit);
            }
            FetchMethod::Tarball => {
                return Err(Error::Fetch(format!(
                    "invalid prior fetch method `tarball` for git source {}",
                    entry.name
                )));
            }
        }
    }

    if fetcher.is_offline() {
        return Err(Error::Fetch(format!(
            "offline mode: no cached artifact is locked for {}",
            entry.name
        )));
    }

    if let Some(url) = archive_url {
        match fetcher.fetch_url(url, None, &format!("forge archive for {}", entry.name)) {
            Ok(artifact) => {
                return Ok(Acquisition::Tarball {
                    artifact,
                    method: FetchMethod::ForgeTarball,
                });
            }
            Err(error) => warnings.push(format!(
                "forge archive fetch failed for {}; trying authenticated or git fallback: {error}",
                entry.name
            )),
        }
    }

    if let Some(github) = github {
        match acquire_github_api_tarball(fetcher, tools, github, commit, None) {
            Ok(artifact) => {
                return Ok(Acquisition::Tarball {
                    artifact,
                    method: FetchMethod::Gh,
                });
            }
            Err(error) => warnings.push(format!(
                "authenticated GitHub tarball fetch failed for {}; trying git clone: {error}",
                entry.name
            )),
        }
    }

    clone_source(temporary_parent, tools, clone_url, locked_ref, commit)
}

fn acquire_with_gh(
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &GithubRepository,
    commit: &str,
    expected_sha256: Option<&str>,
) -> Result<CachedArtifact> {
    let endpoint = format!("repos/{}/{}/tarball/{commit}", github.owner, github.repo);
    let bytes = tools.gh_api_bytes(&endpoint)?;
    fetcher
        .cache()
        .put_bytes(&format!("gh:{endpoint}"), &bytes, expected_sha256)
}

fn acquire_github_api_tarball(
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &GithubRepository,
    commit: &str,
    expected_sha256: Option<&str>,
) -> Result<CachedArtifact> {
    let mut failures = Vec::new();
    if tools.gh_authenticated() {
        match acquire_with_gh(fetcher, tools, github, commit, expected_sha256) {
            Ok(artifact) => return Ok(artifact),
            Err(error) => failures.push(format!("gh: {error}")),
        }
    }
    let url = github_api_tarball_url(github, commit);
    if let Some(token) = env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
    {
        match fetcher.fetch_url_with_bearer(
            &url,
            &token,
            expected_sha256,
            &format!("GitHub API archive {}/{}", github.owner, github.repo),
        ) {
            Ok(artifact) => return Ok(artifact),
            Err(error) => failures.push(format!("token REST: {error}")),
        }
    }
    fetcher
        .fetch_url(
            &url,
            expected_sha256,
            &format!(
                "anonymous GitHub API archive {}/{}",
                github.owner, github.repo
            ),
        )
        .map_err(|error| {
            let rest_error = if error.to_string().contains("HTTP 403") {
                format!("{error}; install `gh` and run `gh auth login`, or set GITHUB_TOKEN")
            } else {
                error.to_string()
            };
            if failures.is_empty() {
                Error::Fetch(rest_error)
            } else {
                failures.push(format!("anonymous REST: {rest_error}"));
                Error::Fetch(format!(
                    "GitHub archive fetch exhausted all available tiers: {}",
                    failures.join("; ")
                ))
            }
        })
}

fn github_api_tarball_url(github: &GithubRepository, commit: &str) -> String {
    let api = if github.host == "github.com" {
        "https://api.github.com".to_owned()
    } else {
        format!("https://{}/api/v3", github.host)
    };
    format!(
        "{api}/repos/{}/{}/tarball/{commit}",
        github.owner, github.repo
    )
}

fn clone_source(
    temporary_parent: &Path,
    tools: &HostTools,
    clone_url: &str,
    reference: Option<&str>,
    commit: &str,
) -> Result<Acquisition> {
    let directory = TempBuilder::new()
        .prefix(".okr-clone-")
        .tempdir_in(temporary_parent)?;
    tools.git_clone_at(clone_url, reference, commit, directory.path())?;
    Ok(Acquisition::Clone { directory })
}

fn extract_tarball(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut top_level: Option<OsString> = None;
    let mut files = BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let mut components = path.components();
        let first = components.next().ok_or_else(|| {
            Error::Io(io::Error::new(io::ErrorKind::InvalidData, "empty tar path"))
        })?;
        let Component::Normal(first) = first else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe tar path {}", path.display()),
            )));
        };
        if let Some(expected) = &top_level {
            if expected != first {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "tarball has more than one top-level directory (`{}` and `{}`)",
                        expected.to_string_lossy(),
                        first.to_string_lossy()
                    ),
                )));
            }
        } else {
            top_level = Some(first.to_owned());
        }
        let mut relative = PathBuf::new();
        for component in components {
            let Component::Normal(part) = component else {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe tar path {}", path.display()),
                )));
            };
            relative.push(part);
        }
        if relative.as_os_str().is_empty() {
            continue;
        }
        let output = destination.join(&relative);
        if entry_type.is_dir() {
            fs::create_dir_all(output)?;
        } else if entry_type.is_file() {
            if !files.insert(relative.clone()) {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate tar path {}", relative.display()),
                )));
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(output)?;
            io::copy(&mut entry, &mut file)?;
        } else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported tar entry type at {}", path.display()),
            )));
        }
    }
    if files.is_empty() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "tarball contains no files",
        )));
    }
    Ok(())
}

fn build_excludes(
    kind: EntryKind,
    include_tests: bool,
    user_patterns: &[String],
) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let defaults = match kind {
        EntryKind::Package => PACKAGE_EXCLUDES,
        EntryKind::Reference => REFERENCE_EXCLUDES,
    };
    for pattern in defaults
        .iter()
        .copied()
        .chain((kind == EntryKind::Package && !include_tests).then_some("tests/**"))
        .chain(user_patterns.iter().map(String::as_str))
    {
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|error| Error::Config(format!("invalid exclude glob `{pattern}`: {error}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| Error::Config(format!("could not build exclude globs: {error}")))
}

fn copy_pruned(source: &Path, destination: &Path, excludes: &GlobSet) -> Result<()> {
    fs::create_dir_all(destination)?;
    let mut paths = Vec::new();
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| Error::Io(io::Error::other(error)))?;
        if entry.path() == source || entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "source contains unsupported non-file entry {}",
                    entry.path().display()
                ),
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| Error::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
        paths.push((normalized_path(relative)?, entry.path().to_owned()));
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, source_file) in paths {
        if excludes.is_match(&relative) {
            continue;
        }
        let target = relative
            .split('/')
            .fold(destination.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source_file, target)?;
    }
    Ok(())
}

fn normalized_path(path: &Path) -> Result<String> {
    let mut normalized = String::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("source path is not normalized: {}", path.display()),
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("source path is not UTF-8: {}", path.display()),
            ))
        })?;
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(part);
    }
    Ok(normalized)
}

fn metadata(
    entry: &ResolvedEntry,
    directory: &Path,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    match entry.kind {
        EntryKind::Package => {
            let path = directory.join("DESCRIPTION");
            let contents = fs::read_to_string(&path).map_err(|error| {
                Error::Fetch(format!(
                    "package {} has no readable DESCRIPTION: {error}",
                    entry.name
                ))
            })?;
            let description = dcf::parse_one(&contents).map_err(|error| {
                Error::Fetch(format!(
                    "package {} has an invalid DESCRIPTION: {error}",
                    entry.name
                ))
            })?;
            let package = description.get("Package").ok_or_else(|| {
                Error::Fetch(format!(
                    "package {} DESCRIPTION has no Package field",
                    entry.name
                ))
            })?;
            if package != entry.name {
                return Err(Error::Fetch(format!(
                    "package directory name `{}` does not match DESCRIPTION Package `{package}`",
                    entry.name
                )));
            }
            let version = description.get("Version").ok_or_else(|| {
                Error::Fetch(format!(
                    "package {} DESCRIPTION has no Version field",
                    entry.name
                ))
            })?;
            if let ResolvedSource::Cran {
                version: resolved, ..
            } = &entry.source
                && version != resolved
            {
                return Err(Error::Fetch(format!(
                    "package {} resolved as version {resolved}, but DESCRIPTION says {version}",
                    entry.name
                )));
            }
            Ok((
                Some(version.to_owned()),
                description.get("License").map(str::to_owned),
                description.get("Title").map(fold_one_line),
            ))
        }
        EntryKind::Reference => Ok((
            None,
            detect_reference_license(directory)?,
            reference_title(directory),
        )),
    }
}

fn fold_one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn detect_reference_license(directory: &Path) -> Result<Option<String>> {
    let mut candidates = fs::read_dir(directory)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("license")
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(fs::DirEntry::file_name);
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    if !candidate
        .file_type()
        .is_ok_and(|file_type| file_type.is_file())
    {
        return Ok(None);
    }
    let fallback = candidate.file_name().to_string_lossy().into_owned();
    let Ok(file) = File::open(candidate.path()) else {
        return Ok(Some(fallback));
    };
    let mut contents = Vec::new();
    if file.take(128 * 1024).read_to_end(&mut contents).is_err() {
        return Ok(Some(fallback));
    }
    let lower = String::from_utf8_lossy(&contents).to_ascii_lowercase();
    let detected = if lower.contains("permission is hereby granted, free of charge") {
        "MIT".to_owned()
    } else if lower.contains("apache license") && lower.contains("version 2.0") {
        "Apache-2.0".to_owned()
    } else if lower.contains("gnu general public license") {
        "GPL".to_owned()
    } else {
        fallback
    };
    Ok(Some(detected))
}

pub(crate) fn reference_title(directory: &Path) -> Option<String> {
    let contents = fs::read_to_string(directory.join("DESCRIPTION")).ok()?;
    dcf::parse_one(&contents)
        .ok()?
        .get("Title")
        .map(fold_one_line)
}

fn clone_cache_key(entry: &ResolvedEntry) -> String {
    match &entry.source {
        ResolvedSource::Git { source, commit, .. } => {
            format!("clone-tree:{source}:{commit}:{}", entry.name)
        }
        _ => format!("clone-tree:invalid:{}", entry.name),
    }
}

fn replace_directory(staging: TempDir, target: &Path) -> Result<()> {
    let parent = target.parent().ok_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("replacement target has no parent: {}", target.display()),
        ))
    })?;
    let staged_path = staging.keep();
    if !target.try_exists()? {
        fs::rename(&staged_path, target)?;
        return Ok(());
    }

    let backup_guard = TempBuilder::new().prefix(".okr-old-").tempdir_in(parent)?;
    let backup = backup_guard.keep();
    fs::remove_dir(&backup)?;
    fs::rename(target, &backup)?;
    if let Err(error) = fs::rename(&staged_path, target) {
        let _ = fs::rename(&backup, target);
        return Err(Error::Io(error));
    }
    fs::remove_dir_all(backup)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;
    use xshell::{Shell, cmd};

    use super::{
        detect_reference_license, extract_tarball, github_api_tarball_url, reference_title, vendor,
    };
    use crate::config::Config;
    use crate::fetch::{Cache, Fetcher};
    use crate::hosttools::HostTools;
    use crate::lock::FetchMethod;
    use crate::resolve::{GithubRepository, Resolution, ResolvedEntry, ResolvedSource};

    #[test]
    fn github_api_archive_urls_cover_dotcom_and_enterprise() {
        let commit = "a".repeat(40);
        assert_eq!(
            github_api_tarball_url(
                &GithubRepository {
                    host: "github.com".into(),
                    owner: "org".into(),
                    repo: "repo".into(),
                },
                &commit,
            ),
            format!("https://api.github.com/repos/org/repo/tarball/{commit}")
        );
        assert_eq!(
            github_api_tarball_url(
                &GithubRepository {
                    host: "github.corp.example".into(),
                    owner: "org".into(),
                    repo: "repo".into(),
                },
                &commit,
            ),
            format!("https://github.corp.example/api/v3/repos/org/repo/tarball/{commit}")
        );
    }

    #[test]
    fn reference_metadata_detection_is_best_effort() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("DESCRIPTION"),
            "Package: contextual\nTitle: Context for\n  Agents\n",
        )
        .unwrap();
        fs::write(directory.path().join("LICENSE.binary"), [0xff, 0xfe]).unwrap();
        assert_eq!(
            reference_title(directory.path()).as_deref(),
            Some("Context for Agents")
        );
        assert_eq!(
            detect_reference_license(directory.path())
                .unwrap()
                .as_deref(),
            Some("LICENSE.binary")
        );
    }

    #[test]
    fn package_tarball_is_stripped_pruned_and_described() {
        let project = tempdir().unwrap();
        let fixture = fixture_path("fixture-repo/2026-06-30/src/contrib/tinyone_1.0.0.tar.gz");
        let config = Config::parse(
            "[project]\nsnapshot = \"2026-06-30\"\n[vendor]\ninclude-tests = false\n[packages]\ntinyone = \"*\"",
        )
        .unwrap();
        let resolution = Resolution {
            entries: vec![ResolvedEntry {
                name: "tinyone".into(),
                kind: crate::config::EntryKind::Package,
                source: ResolvedSource::Cran {
                    version: "1.0.0".into(),
                    url: format!("file://{}", fixture.display()),
                },
                exclude: Vec::new(),
                include_tests: None,
                artifact_sha256: None,
                preferred_fetch_method: None,
            }],
            warnings: Vec::new(),
        };
        let fetcher = Fetcher::new(Cache::new(project.path().join("cache")), false).unwrap();
        let result = vendor(
            project.path(),
            &config,
            &resolution,
            &fetcher,
            &HostTools::new(),
        )
        .unwrap();
        let tree = project.path().join("deps-src/tinyone");
        assert!(tree.join("DESCRIPTION").is_file());
        assert!(tree.join("R/hello.R").is_file());
        assert!(tree.join("man/hello.Rd").is_file());
        assert!(!tree.join("data/answers.csv").exists());
        assert!(!tree.join("tests/testthat.R").exists());
        assert_eq!(result.entries[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(result.entries[0].license.as_deref(), Some("MIT"));
        assert_eq!(result.entries[0].fetch_method, FetchMethod::Tarball);
    }

    #[test]
    fn extraction_ignores_pax_global_metadata_before_the_source_root() {
        let directory = tempdir().unwrap();
        let archive_path = directory.path().join("github-archive.tar.gz");
        let file = fs::File::create(&archive_path).unwrap();
        let gzip = GzEncoder::new(file, Compression::default());
        let mut archive = tar::Builder::new(gzip);

        let pax = b"52 comment=37b74f85e62680b9d4523b0b4c0d9bfa0403d299\n";
        let mut pax_header = tar::Header::new_ustar();
        pax_header.set_entry_type(tar::EntryType::XGlobalHeader);
        pax_header.set_size(pax.len() as u64);
        pax_header.set_cksum();
        archive
            .append_data(&mut pax_header, "pax_global_header", &pax[..])
            .unwrap();

        let description = b"Package: ggsci\nVersion: 4.0.0\n";
        let mut file_header = tar::Header::new_ustar();
        file_header.set_entry_type(tar::EntryType::file());
        file_header.set_mode(0o644);
        file_header.set_size(description.len() as u64);
        file_header.set_cksum();
        archive
            .append_data(
                &mut file_header,
                "ggsci-37b74f85e62680b9d4523b0b4c0d9bfa0403d299/DESCRIPTION",
                &description[..],
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let extracted = directory.path().join("extracted");
        fs::create_dir(&extracted).unwrap();
        extract_tarball(&archive_path, &extracted).unwrap();

        assert_eq!(
            fs::read(extracted.join("DESCRIPTION")).unwrap(),
            description
        );
        assert!(!extracted.join("pax_global_header").exists());
    }

    #[test]
    fn reference_clone_keeps_non_r_context_and_replays_offline() {
        let tools = HostTools::new();
        if !tools.git_available() {
            return;
        }
        let project = tempdir().unwrap();
        let repository = project.path().join("reference-source");
        copy_fixture_tree(&fixture_path("reference-repo"), &repository);
        let shell = Shell::new().unwrap();
        cmd!(shell, "git init -q -b main {repository}")
            .run()
            .unwrap();
        cmd!(shell, "git -C {repository} config user.name okr-test")
            .run()
            .unwrap();
        cmd!(
            shell,
            "git -C {repository} config user.email okr@example.test"
        )
        .run()
        .unwrap();
        cmd!(shell, "git -C {repository} add .").run().unwrap();
        cmd!(shell, "git -C {repository} commit -q -m fixture")
            .run()
            .unwrap();
        let commit = cmd!(shell, "git -C {repository} rev-parse HEAD")
            .read()
            .unwrap();
        let source = format!("file://{}", repository.display());
        let config = Config::parse(&format!(
            "[references]\nstandards = \"git::{source}@{commit}\""
        ))
        .unwrap();
        let entry = ResolvedEntry {
            name: "standards".into(),
            kind: crate::config::EntryKind::Reference,
            source: ResolvedSource::Git {
                source: format!("git::{source}"),
                clone_url: source,
                requested_ref: Some(commit.clone()),
                locked_ref: Some(commit.clone()),
                commit,
                archive_url: None,
                github: None,
            },
            exclude: Vec::new(),
            include_tests: None,
            artifact_sha256: None,
            preferred_fetch_method: None,
        };
        let cache = Cache::new(project.path().join("cache"));
        let online = vendor(
            project.path(),
            &config,
            &Resolution {
                entries: vec![entry.clone()],
                warnings: Vec::new(),
            },
            &Fetcher::new(cache.clone(), false).unwrap(),
            &tools,
        )
        .unwrap();
        let tree = project.path().join("deps-src/standards");
        assert!(tree.join("docs/guide.md").is_file());
        assert!(tree.join("data/example.json").is_file());
        assert!(!tree.join(".git").exists());
        assert!(!tree.join(".gitattributes").exists());
        assert_eq!(online.entries[0].license.as_deref(), Some("MIT"));
        assert_eq!(online.entries[0].fetch_method, FetchMethod::GitClone);

        let mut offline_entry = entry;
        offline_entry.artifact_sha256 = Some(online.entries[0].artifact_sha256.clone());
        offline_entry.preferred_fetch_method = Some(FetchMethod::GitClone);
        let offline = vendor(
            project.path(),
            &config,
            &Resolution {
                entries: vec![offline_entry],
                warnings: Vec::new(),
            },
            &Fetcher::new(cache, true).unwrap(),
            &tools,
        )
        .unwrap();
        assert_eq!(online.entries[0].tree, offline.entries[0].tree);
    }

    #[test]
    fn full_vendor_root_replacement_removes_stale_entries() {
        let project = tempdir().unwrap();
        let old = project.path().join("deps-src/stale");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("old"), b"old").unwrap();
        let fixture = fixture_path("forge/package-archive.tar.gz");
        let config = Config::parse(
            "[packages]\ntinytwo = { url = \"https://example.test/tinytwo.tar.gz\", sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }",
        )
        .unwrap();
        let resolution = Resolution {
            entries: vec![ResolvedEntry {
                name: "tinytwo".into(),
                kind: crate::config::EntryKind::Package,
                source: ResolvedSource::Tarball {
                    source: "url::fixture".into(),
                    url: format!("file://{}", fixture.display()),
                    reference: None,
                },
                exclude: Vec::new(),
                include_tests: None,
                artifact_sha256: None,
                preferred_fetch_method: None,
            }],
            warnings: Vec::new(),
        };
        vendor(
            project.path(),
            &config,
            &resolution,
            &Fetcher::new(Cache::new(project.path().join("cache")), false).unwrap(),
            &HostTools::new(),
        )
        .unwrap();
        assert!(!project.path().join("deps-src/stale").exists());
        assert!(project.path().join("deps-src/tinytwo/R/hello.R").is_file());
        assert!(
            project
                .path()
                .join("deps-src/tinytwo/tests/testthat.R")
                .is_file()
        );
        assert!(!project.path().join("deps-src/tinytwo/data").exists());
    }

    #[test]
    fn public_forge_archive_path_does_not_invoke_git() {
        let project = tempdir().unwrap();
        let fixture = fixture_path("forge/package-archive.tar.gz");
        let config = Config::parse("[packages]\ntinytwo = \"owner/tinytwo@deadbeef\"").unwrap();
        let resolution = Resolution {
            entries: vec![ResolvedEntry {
                name: "tinytwo".into(),
                kind: crate::config::EntryKind::Package,
                source: ResolvedSource::Git {
                    source: "github::owner/tinytwo".into(),
                    clone_url: "unusable://git-is-not-needed".into(),
                    requested_ref: Some("deadbeef".into()),
                    locked_ref: Some("deadbeef".into()),
                    commit: "f".repeat(40),
                    archive_url: Some(format!("file://{}", fixture.display())),
                    github: None,
                },
                exclude: Vec::new(),
                include_tests: None,
                artifact_sha256: None,
                preferred_fetch_method: None,
            }],
            warnings: Vec::new(),
        };
        let result = vendor(
            project.path(),
            &config,
            &resolution,
            &Fetcher::new(Cache::new(project.path().join("cache")), false).unwrap(),
            &HostTools::new(),
        )
        .unwrap();
        assert_eq!(result.entries[0].fetch_method, FetchMethod::ForgeTarball);
        assert!(
            project
                .path()
                .join("deps-src/tinytwo/DESCRIPTION")
                .is_file()
        );
    }

    fn fixture_path(relative: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(relative)
    }

    fn copy_fixture_tree(source: &Path, destination: &Path) {
        for entry in walkdir::WalkDir::new(source) {
            let entry = entry.unwrap();
            let relative = entry.path().strip_prefix(source).unwrap();
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(target).unwrap();
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}
