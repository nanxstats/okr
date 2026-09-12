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

/// Unix file type bits carried by zip entries created on Unix hosts.
const UNIX_TYPE_MASK: u32 = 0o170000;
const UNIX_DIRECTORY: u32 = 0o040000;
const UNIX_REGULAR: u32 = 0o100000;
const UNIX_SYMLINK: u32 = 0o120000;

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
    let (raw, archive_method) = match acquisition {
        Acquisition::Archive { artifact, method } => {
            let raw = TempBuilder::new()
                .prefix(".okr-extract-")
                .tempdir_in(temporary_parent)?;
            extract_archive(&artifact.path, raw.path()).map_err(|error| {
                Error::Fetch(format!(
                    "could not extract source for {} from {}: {error}",
                    entry.name,
                    artifact.path.display()
                ))
            })?;
            (raw, Some(method))
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
    if let Some(expected) = &entry.expected_tree_digest
        && tree.digest != *expected
    {
        return Err(Error::Fetch(format!(
            "source tree for {} does not match okr.lock: expected {expected}, found {}; the locked source changed upstream. If that is intended, delete okr.lock and run `okr sync` again",
            entry.name, tree.digest
        )));
    }
    let fetch_method = match archive_method {
        Some(method) => method,
        None => {
            fetcher
                .cache()
                .put_normalized_tree(&clone_cache_key(entry), destination)?;
            FetchMethod::GitClone
        }
    };

    Ok(VendoredEntry {
        name: entry.name.clone(),
        kind: entry.kind,
        version,
        license,
        title,
        fetch_method,
        tree,
    })
}

enum Acquisition {
    /// A cached archive in any supported format, extracted before pruning.
    Archive {
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
        ResolvedSource::Cran { url, .. } | ResolvedSource::Archive { url, .. } => {
            let artifact = fetcher.fetch_url(
                url,
                entry.declared_sha256.as_deref(),
                &format!("source for {}", entry.name),
            )?;
            Ok(Acquisition::Archive {
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
    // Replay the locked fetch method. Each method has its own cache key, so a
    // cached artifact is found without a digest and offline rebuilds never
    // switch methods.
    if let Some(method) = entry.preferred_fetch_method {
        match method {
            FetchMethod::ForgeTarball => {
                let url = archive_url.ok_or_else(|| {
                    Error::Fetch(format!(
                        "cannot replay forge-tarball fetch for {}: no forge archive URL",
                        entry.name
                    ))
                })?;
                let artifact =
                    fetcher.fetch_url(url, None, &format!("forge archive for {}", entry.name))?;
                return Ok(Acquisition::Archive { artifact, method });
            }
            FetchMethod::Gh => {
                let github = github.ok_or_else(|| {
                    Error::Fetch(format!(
                        "cannot replay gh fetch for {}: source is not GitHub",
                        entry.name
                    ))
                })?;
                if let Some(artifact) = cached_github_api_tarball(fetcher, github, commit)? {
                    return Ok(Acquisition::Archive { artifact, method });
                }
                if fetcher.is_offline() {
                    return Err(Error::Fetch(format!(
                        "offline mode: missing cached GitHub archive for {}",
                        entry.name
                    )));
                }
                let artifact = acquire_github_api_tarball(fetcher, tools, github, commit)?;
                return Ok(Acquisition::Archive { artifact, method });
            }
            FetchMethod::GitClone => {
                if let Some(artifact) = fetcher.cache().lookup(&clone_cache_key(entry))? {
                    return Ok(Acquisition::Archive { artifact, method });
                }
                if fetcher.is_offline() {
                    return Err(Error::Fetch(format!(
                        "offline mode: missing cached clone archive for {}",
                        entry.name
                    )));
                }
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
                return Ok(Acquisition::Archive {
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
        match acquire_github_api_tarball(fetcher, tools, github, commit) {
            Ok(artifact) => {
                return Ok(Acquisition::Archive {
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

fn gh_tarball_endpoint(github: &GithubRepository, commit: &str) -> String {
    format!("repos/{}/{}/tarball/{commit}", github.owner, github.repo)
}

fn acquire_with_gh(
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &GithubRepository,
    commit: &str,
) -> Result<CachedArtifact> {
    let endpoint = gh_tarball_endpoint(github, commit);
    let bytes = tools.gh_api_bytes(&endpoint)?;
    fetcher
        .cache()
        .put_bytes(&format!("gh:{endpoint}"), &bytes, None)
}

/// Find a GitHub API archive for this commit cached by any access tier.
fn cached_github_api_tarball(
    fetcher: &Fetcher,
    github: &GithubRepository,
    commit: &str,
) -> Result<Option<CachedArtifact>> {
    if let Some(artifact) = fetcher
        .cache()
        .lookup(&format!("gh:{}", gh_tarball_endpoint(github, commit)))?
    {
        return Ok(Some(artifact));
    }
    fetcher.cached_url(&github_api_tarball_url(github, commit))
}

fn acquire_github_api_tarball(
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &GithubRepository,
    commit: &str,
) -> Result<CachedArtifact> {
    let mut failures = Vec::new();
    if tools.gh_authenticated() {
        match acquire_with_gh(fetcher, tools, github, commit) {
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
            &format!("GitHub API archive {}/{}", github.owner, github.repo),
        ) {
            Ok(artifact) => return Ok(artifact),
            Err(error) => failures.push(format!("token REST: {error}")),
        }
    }
    fetcher
        .fetch_url(
            &url,
            None,
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

/// The archive container formats `okr` can extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    /// A gzip-compressed tar archive: CRAN and `url::` tarballs, forge and
    /// GitHub API archives, and the normalized clone cache.
    GzipTarball,
    /// A zip archive declared through a `url::` source.
    Zip,
}

impl ArchiveFormat {
    /// Identify an archive by its leading bytes rather than by a file name.
    /// Cached artifacts carry no extension, and sniffing also produces a clear
    /// error when a server answers with something other than an archive.
    fn detect(path: &Path) -> Result<Self> {
        let mut magic = Vec::with_capacity(4);
        File::open(path)?.take(4).read_to_end(&mut magic)?;
        match magic.as_slice() {
            [0x1f, 0x8b, ..] => Ok(Self::GzipTarball),
            [b'P', b'K', 0x03, 0x04] | [b'P', b'K', 0x05, 0x06] | [b'P', b'K', 0x07, 0x08] => {
                Ok(Self::Zip)
            }
            _ => Err(invalid_archive(
                "unrecognized archive format; expected a gzip tarball (.tar.gz or .tgz) or a zip archive (.zip)",
            )),
        }
    }
}

/// Extract an archive of any supported format below `destination`, removing
/// its single wrapper directory.
fn extract_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    match ArchiveFormat::detect(archive_path)? {
        ArchiveFormat::GzipTarball => extract_tarball(archive_path, destination),
        ArchiveFormat::Zip => extract_zip(archive_path, destination),
    }
}

/// The wrapper directory and file set shared by every entry of one archive.
///
/// Tar and zip extraction both route their entries through this type so the
/// wrapper-stripping rule, path safety checks, and duplicate detection are
/// identical across formats.
#[derive(Debug, Default)]
struct ArchiveLayout {
    wrapper: Option<OsString>,
    files: BTreeSet<PathBuf>,
}

impl ArchiveLayout {
    /// Validate an entry path and return it relative to the wrapper
    /// directory. The wrapper directory entry itself yields `None`.
    fn relative(&mut self, path: &Path, is_directory: bool) -> Result<Option<PathBuf>> {
        let mut components = path.components();
        let Some(first) = components.next() else {
            return Err(invalid_archive("empty archive path"));
        };
        let Component::Normal(first) = first else {
            return Err(unsafe_archive_path(path));
        };
        match &self.wrapper {
            Some(wrapper) if wrapper != first => {
                return Err(invalid_archive(format!(
                    "archive must contain a single top-level directory, found `{}` and `{}`",
                    wrapper.to_string_lossy(),
                    first.to_string_lossy()
                )));
            }
            Some(_) => {}
            None => self.wrapper = Some(first.to_owned()),
        }
        let mut relative = PathBuf::new();
        for component in components {
            let Component::Normal(part) = component else {
                return Err(unsafe_archive_path(path));
            };
            relative.push(part);
        }
        if relative.as_os_str().is_empty() {
            if is_directory {
                return Ok(None);
            }
            return Err(invalid_archive(format!(
                "archive must contain a single top-level directory, found top-level file `{}`",
                first.to_string_lossy()
            )));
        }
        Ok(Some(relative))
    }

    /// Record a file beneath the wrapper directory, rejecting duplicates.
    fn record_file(&mut self, relative: &Path) -> Result<()> {
        if self.files.insert(relative.to_path_buf()) {
            Ok(())
        } else {
            Err(invalid_archive(format!(
                "duplicate archive path {}",
                relative.display()
            )))
        }
    }

    /// Require that the archive produced at least one file.
    fn finish(self) -> Result<()> {
        if self.files.is_empty() {
            return Err(invalid_archive("archive contains no files"));
        }
        Ok(())
    }
}

fn extract_tarball(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut layout = ArchiveLayout::default();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let Some(relative) = layout.relative(&path, entry_type.is_dir())? else {
            continue;
        };
        let output = destination.join(&relative);
        if entry_type.is_dir() {
            fs::create_dir_all(output)?;
        } else if entry_type.is_file() || entry_type.is_symlink() {
            layout.record_file(&relative)?;
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            if entry_type.is_file() {
                let mut file = File::create(output)?;
                io::copy(&mut entry, &mut file)?;
            } else {
                let target = entry.link_name_bytes().ok_or_else(|| {
                    invalid_archive(format!("symbolic link has no target at {}", path.display()))
                })?;
                fs::write(output, target.as_ref())?;
            }
        } else {
            return Err(invalid_archive(format!(
                "unsupported tar entry type at {}",
                path.display()
            )));
        }
    }
    layout.finish()
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(archive_path)?).map_err(zip_error)?;
    let mut layout = ArchiveLayout::default();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        let name = entry.name().to_owned();
        let unix_type = entry.unix_mode().map_or(0, |mode| mode & UNIX_TYPE_MASK);
        if !matches!(unix_type, 0 | UNIX_DIRECTORY | UNIX_REGULAR | UNIX_SYMLINK) {
            return Err(invalid_archive(format!(
                "unsupported zip entry type at {name}"
            )));
        }
        let is_directory = entry.is_dir() || unix_type == UNIX_DIRECTORY;
        let Some(relative) = layout.relative(Path::new(&name), is_directory)? else {
            continue;
        };
        let output = destination.join(&relative);
        if is_directory {
            fs::create_dir_all(output)?;
            continue;
        }
        // A zip symbolic link stores its target as the entry content, so
        // copying the content materializes the link exactly as tar
        // extraction does.
        layout.record_file(&relative)?;
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(output)?;
        io::copy(&mut entry, &mut file)?;
    }
    layout.finish()
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Io(io::Error::from(error))
}

fn invalid_archive(message: impl Into<String>) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

fn unsafe_archive_path(path: &Path) -> Error {
    invalid_archive(format!("unsafe archive path {}", path.display()))
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
    enum SourceContent {
        File(PathBuf),
        SymbolicLink(Vec<u8>),
    }

    fs::create_dir_all(destination)?;
    let mut paths = Vec::new();
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| Error::Io(io::Error::other(error)))?;
        if entry.path() == source || entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| Error::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
        let normalized = normalized_path(relative)?;
        let content = if entry.file_type().is_file() {
            SourceContent::File(entry.path().to_owned())
        } else if entry.file_type().is_symlink() {
            SourceContent::SymbolicLink(symbolic_link_bytes(entry.path())?)
        } else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "source contains unsupported non-file entry {}",
                    entry.path().display()
                ),
            )));
        };
        paths.push((normalized, content));
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, content) in paths {
        if excludes.is_match(&relative) {
            continue;
        }
        let target = relative
            .split('/')
            .fold(destination.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        match content {
            SourceContent::File(source_file) => {
                fs::copy(source_file, target)?;
            }
            SourceContent::SymbolicLink(link_target) => fs::write(target, link_target)?,
        }
    }
    Ok(())
}

fn symbolic_link_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        let target = target.to_str().ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "symbolic link target is not valid UTF-8 at {}",
                    path.display()
                ),
            ))
        })?;
        Ok(target.as_bytes().to_vec())
    }
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
                if error.kind() == io::ErrorKind::NotFound {
                    missing_package_description(entry)
                } else {
                    Error::Fetch(format!(
                        "package {} has an unreadable DESCRIPTION: {error}",
                        entry.name
                    ))
                }
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

fn missing_package_description(entry: &ResolvedEntry) -> Error {
    let mut message = format!(
        "source `{}` was declared in `[packages]`, but it has no `DESCRIPTION` file and may not be an R package",
        entry.name
    );
    if !matches!(entry.source, ResolvedSource::Cran { .. }) {
        message.push_str(&format!(
            ". If it is a non-R project, move its `{}` declaration from `[packages]` to `[references]` in `okr.toml` (preserving the value)",
            entry.name
        ));
        if let ResolvedSource::Git {
            source,
            requested_ref,
            ..
        } = &entry.source
        {
            let source = source.strip_prefix("github::").unwrap_or(source);
            let reference = requested_ref
                .as_deref()
                .map(|reference| format!("@{reference}"))
                .unwrap_or_default();
            message.push_str(&format!(
                ", or remove it from `[packages]` and run `okr add {source}{reference} --reference`"
            ));
        }
    }
    Error::Fetch(message)
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
    use std::path::{Path, PathBuf};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;
    use xshell::{Shell, cmd};

    use super::{
        ArchiveFormat, ArchiveLayout, build_excludes, copy_pruned, detect_reference_license,
        extract_archive, github_api_tarball_url, metadata, reference_title, vendor,
    };
    use crate::config::{Config, EntryKind};
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
    fn missing_package_description_suggests_reference_recovery() {
        let directory = tempdir().unwrap();
        let entry = ResolvedEntry {
            name: "okr".into(),
            kind: crate::config::EntryKind::Package,
            source: ResolvedSource::Git {
                source: "github::nanxstats/okr".into(),
                clone_url: "https://github.com/nanxstats/okr.git".into(),
                requested_ref: None,
                locked_ref: None,
                commit: "a".repeat(40),
                archive_url: None,
                github: None,
            },
            exclude: Vec::new(),
            include_tests: None,
            declared_sha256: None,
            expected_tree_digest: None,
            preferred_fetch_method: None,
        };

        let error = metadata(&entry, directory.path()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "source `okr` was declared in `[packages]`, but it has no `DESCRIPTION` file and may not be an R package. If it is a non-R project, move its `okr` declaration from `[packages]` to `[references]` in `okr.toml` (preserving the value), or remove it from `[packages]` and run `okr add nanxstats/okr --reference`"
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
                declared_sha256: None,
                expected_tree_digest: None,
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
        extract_archive(&archive_path, &extracted).unwrap();

        assert_eq!(
            fs::read(extracted.join("DESCRIPTION")).unwrap(),
            description
        );
        assert!(!extracted.join("pax_global_header").exists());
    }

    #[test]
    fn extraction_materializes_symbolic_links_as_regular_files() {
        let directory = tempdir().unwrap();
        let archive_path = directory.path().join("reference-archive.tar.gz");
        let file = fs::File::create(&archive_path).unwrap();
        let gzip = GzEncoder::new(file, Compression::default());
        let mut archive = tar::Builder::new(gzip);

        let contents = b"Package: pkgA\n";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_entry_type(tar::EntryType::file());
        file_header.set_mode(0o644);
        file_header.set_size(contents.len() as u64);
        file_header.set_cksum();
        archive
            .append_data(
                &mut file_header,
                "source/tests/Pkgs/xDir/pkg/DESCRIPTION",
                &contents[..],
            )
            .unwrap();

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_mode(0o777);
        link_header.set_size(0);
        archive
            .append_link(&mut link_header, "source/tests/Pkgs/pkgA", "xDir/pkg")
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let extracted = directory.path().join("extracted");
        fs::create_dir(&extracted).unwrap();
        extract_archive(&archive_path, &extracted).unwrap();

        let link = extracted.join("tests/Pkgs/pkgA");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        assert_eq!(fs::read(link).unwrap(), b"xDir/pkg");
    }

    #[cfg(unix)]
    #[test]
    fn pruning_materializes_clone_symbolic_links_as_regular_files() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::create_dir_all(source.join("tests/Pkgs/xDir/pkg")).unwrap();
        fs::write(
            source.join("tests/Pkgs/xDir/pkg/DESCRIPTION"),
            b"Package: pkgA\n",
        )
        .unwrap();
        symlink("xDir/pkg", source.join("tests/Pkgs/pkgA")).unwrap();

        let excludes = build_excludes(EntryKind::Reference, true, &[]).unwrap();
        copy_pruned(&source, &destination, &excludes).unwrap();

        let link = destination.join("tests/Pkgs/pkgA");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        assert_eq!(fs::read(link).unwrap(), b"xDir/pkg");
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
            declared_sha256: None,
            expected_tree_digest: None,
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
        offline_entry.expected_tree_digest = Some(online.entries[0].tree.digest.clone());
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
                source: ResolvedSource::Archive {
                    source: "url::fixture".into(),
                    url: format!("file://{}", fixture.display()),
                    reference: None,
                },
                exclude: Vec::new(),
                include_tests: None,
                declared_sha256: None,
                expected_tree_digest: None,
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
    fn rebuilt_tree_must_reproduce_the_locked_tree_digest() {
        let project = tempdir().unwrap();
        let fixture = fixture_path("fixture-repo/2026-06-30/src/contrib/tinyone_1.0.0.tar.gz");
        let config =
            Config::parse("[project]\nsnapshot = \"2026-06-30\"\n[packages]\ntinyone = \"*\"")
                .unwrap();
        let entry = ResolvedEntry {
            name: "tinyone".into(),
            kind: crate::config::EntryKind::Package,
            source: ResolvedSource::Cran {
                version: "1.0.0".into(),
                url: format!("file://{}", fixture.display()),
            },
            exclude: Vec::new(),
            include_tests: None,
            declared_sha256: None,
            expected_tree_digest: Some(format!("sha256:{}", "0".repeat(64))),
            preferred_fetch_method: Some(FetchMethod::Tarball),
        };
        let resolution = |entry: ResolvedEntry| Resolution {
            entries: vec![entry],
            warnings: Vec::new(),
        };
        let cache = Cache::new(project.path().join("cache"));
        let online = Fetcher::new(cache.clone(), false).unwrap();

        let error = vendor(
            project.path(),
            &config,
            &resolution(entry.clone()),
            &online,
            &HostTools::new(),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 3);
        assert!(
            error
                .to_string()
                .contains("source tree for tinyone does not match okr.lock"),
            "{error}"
        );
        assert!(!project.path().join("deps-src").exists());

        let mut unlocked = entry.clone();
        unlocked.expected_tree_digest = None;
        let first = vendor(
            project.path(),
            &config,
            &resolution(unlocked),
            &online,
            &HostTools::new(),
        )
        .unwrap();

        let mut replay = entry;
        replay.expected_tree_digest = Some(first.entries[0].tree.digest.clone());
        let offline = vendor(
            project.path(),
            &config,
            &resolution(replay),
            &Fetcher::new(cache, true).unwrap(),
            &HostTools::new(),
        )
        .unwrap();
        assert_eq!(offline.entries[0].tree, first.entries[0].tree);
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
                declared_sha256: None,
                expected_tree_digest: None,
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

    enum ZipEntry<'a> {
        Directory(&'a str),
        File(&'a str, &'a [u8]),
        Symlink(&'a str, &'a str),
    }

    fn write_zip(path: &Path, entries: &[ZipEntry<'_>]) {
        use std::io::Write as _;

        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for entry in entries {
            match entry {
                ZipEntry::Directory(name) => writer.add_directory(*name, options).unwrap(),
                ZipEntry::File(name, contents) => {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(contents).unwrap();
                }
                ZipEntry::Symlink(name, target) => {
                    writer.add_symlink(*name, *target, options).unwrap();
                }
            }
        }
        writer.finish().unwrap();
    }

    #[test]
    fn archive_format_is_detected_from_leading_bytes() {
        assert_eq!(
            ArchiveFormat::detect(&fixture_path("forge/package-archive.tar.gz")).unwrap(),
            ArchiveFormat::GzipTarball
        );
        assert_eq!(
            ArchiveFormat::detect(&fixture_path("zip/reference-archive.zip")).unwrap(),
            ArchiveFormat::Zip
        );

        let directory = tempdir().unwrap();
        for (name, contents) in [
            ("html", &b"<html>not found</html>"[..]),
            ("empty", &b""[..]),
            ("short", &b"PK"[..]),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, contents).unwrap();
            let error = extract_archive(&path, directory.path()).unwrap_err();
            assert!(
                error.to_string().contains("unrecognized archive format"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn archive_layout_requires_one_wrapper_directory_and_unique_files() {
        let mut layout = ArchiveLayout::default();
        assert_eq!(layout.relative(Path::new("source"), true).unwrap(), None);
        assert_eq!(
            layout
                .relative(Path::new("source/R/hello.R"), false)
                .unwrap(),
            Some(PathBuf::from("R/hello.R"))
        );
        layout.record_file(Path::new("R/hello.R")).unwrap();
        let duplicate = layout.record_file(Path::new("R/hello.R")).unwrap_err();
        assert!(
            duplicate.to_string().contains("duplicate archive path"),
            "{duplicate}"
        );
        for (path, guidance) in [
            ("other/README.md", "single top-level directory"),
            ("README.md", "single top-level directory"),
            ("../escape", "unsafe archive path"),
            ("/absolute", "unsafe archive path"),
            ("source/../escape", "unsafe archive path"),
            ("", "empty archive path"),
        ] {
            let error = layout.relative(Path::new(path), false).unwrap_err();
            assert!(error.to_string().contains(guidance), "`{path}`: {error}");
        }

        let top_level_file = ArchiveLayout::default()
            .relative(Path::new("README.md"), false)
            .unwrap_err();
        assert!(
            top_level_file
                .to_string()
                .contains("top-level file `README.md`"),
            "{top_level_file}"
        );
        let empty = ArchiveLayout::default().finish().unwrap_err();
        assert!(empty.to_string().contains("contains no files"), "{empty}");
    }

    #[test]
    fn zip_extraction_strips_the_wrapper_and_materializes_symbolic_links() {
        let directory = tempdir().unwrap();
        let archive_path = directory.path().join("reference-archive.zip");
        write_zip(
            &archive_path,
            &[
                ZipEntry::Directory("source"),
                ZipEntry::File("source/README.md", b"# Notes\n"),
                ZipEntry::Directory("source/tests/Pkgs/xDir/pkg"),
                ZipEntry::File("source/tests/Pkgs/xDir/pkg/DESCRIPTION", b"Package: pkgA\n"),
                ZipEntry::Symlink("source/tests/Pkgs/pkgA", "xDir/pkg"),
            ],
        );

        let extracted = directory.path().join("extracted");
        fs::create_dir(&extracted).unwrap();
        extract_archive(&archive_path, &extracted).unwrap();

        assert_eq!(fs::read(extracted.join("README.md")).unwrap(), b"# Notes\n");
        assert_eq!(
            fs::read(extracted.join("tests/Pkgs/xDir/pkg/DESCRIPTION")).unwrap(),
            b"Package: pkgA\n"
        );
        let link = extracted.join("tests/Pkgs/pkgA");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        assert_eq!(fs::read(link).unwrap(), b"xDir/pkg");
        assert!(!extracted.join("source").exists());
    }

    #[test]
    fn zip_extraction_rejects_unsafe_and_ambiguous_layouts() {
        let directory = tempdir().unwrap();
        let cases: [(&str, &[ZipEntry<'_>], &str); 4] = [
            (
                "traversal",
                &[ZipEntry::File("source/../escape.txt", b"escaped")],
                "unsafe archive path",
            ),
            (
                "absolute",
                &[ZipEntry::File("/escape.txt", b"escaped")],
                "unsafe archive path",
            ),
            (
                "two-roots",
                &[
                    ZipEntry::File("one/README.md", b"one"),
                    ZipEntry::File("two/README.md", b"two"),
                ],
                "single top-level directory",
            ),
            (
                "empty",
                &[ZipEntry::Directory("source")],
                "contains no files",
            ),
        ];
        for (name, entries, guidance) in cases {
            let archive_path = directory.path().join(format!("{name}.zip"));
            write_zip(&archive_path, entries);
            let extracted = directory.path().join(name);
            fs::create_dir(&extracted).unwrap();
            let error = extract_archive(&archive_path, &extracted).unwrap_err();
            assert!(error.to_string().contains(guidance), "{name}: {error}");
            assert!(!directory.path().join("escape.txt").exists());
        }
    }

    #[test]
    fn reference_zip_archive_is_vendored_and_replays_offline() {
        let project = tempdir().unwrap();
        let fixture = fixture_path("zip/reference-archive.zip");
        let sha256 = crate::digest::sha256_file(&fixture).unwrap();
        let config = Config::parse(&format!(
            "[references]\nnotes = {{ url = \"https://example.test/notes-1.0.zip\", sha256 = \"{sha256}\" }}"
        ))
        .unwrap();
        let entry = ResolvedEntry {
            name: "notes".into(),
            kind: EntryKind::Reference,
            source: ResolvedSource::Archive {
                source: "url::https://example.test/notes-1.0.zip".into(),
                url: format!("file://{}", fixture.display()),
                reference: None,
            },
            exclude: Vec::new(),
            include_tests: None,
            declared_sha256: Some(sha256),
            expected_tree_digest: None,
            preferred_fetch_method: None,
        };
        let resolution = |entry: ResolvedEntry| Resolution {
            entries: vec![entry],
            warnings: Vec::new(),
        };
        let cache = Cache::new(project.path().join("cache"));

        let online = vendor(
            project.path(),
            &config,
            &resolution(entry.clone()),
            &Fetcher::new(cache.clone(), false).unwrap(),
            &HostTools::new(),
        )
        .unwrap();
        let tree = project.path().join("deps-src/notes");
        assert!(tree.join("README.md").is_file());
        assert!(tree.join("CHANGES").is_file());
        assert!(tree.join("docs/guide.md").is_file());
        assert!(tree.join("tools/check.py").is_file());
        assert!(!tree.join("notes-1.0").exists());
        assert_eq!(online.entries[0].version, None);
        assert_eq!(online.entries[0].license.as_deref(), Some("MIT"));
        assert_eq!(online.entries[0].fetch_method, FetchMethod::Tarball);

        let mut replay = entry;
        replay.expected_tree_digest = Some(online.entries[0].tree.digest.clone());
        replay.preferred_fetch_method = Some(FetchMethod::Tarball);
        let offline = vendor(
            project.path(),
            &config,
            &resolution(replay),
            &Fetcher::new(cache, true).unwrap(),
            &HostTools::new(),
        )
        .unwrap();
        assert_eq!(offline.entries[0].tree, online.entries[0].tree);
    }

    #[test]
    fn package_zip_archive_is_pruned_like_a_tarball() {
        let project = tempdir().unwrap();
        let source = fixture_path("sources/tinytwo");
        let files = [
            "DESCRIPTION",
            "NAMESPACE",
            "R/hello.R",
            "man/hello.Rd",
            "data/answers.csv",
            "tests/testthat.R",
        ]
        .map(|relative| {
            (
                format!("tinytwo-2.0.0/{relative}"),
                fs::read(source.join(relative)).unwrap(),
            )
        });
        let entries = files
            .iter()
            .map(|(name, contents)| ZipEntry::File(name, contents))
            .collect::<Vec<_>>();
        let archive_path = project.path().join("tinytwo.zip");
        write_zip(&archive_path, &entries);
        let sha256 = crate::digest::sha256_file(&archive_path).unwrap();
        let config = Config::parse(&format!(
            "[vendor]\ninclude-tests = false\n[packages]\ntinytwo = {{ url = \"https://example.test/tinytwo.zip\", sha256 = \"{sha256}\" }}"
        ))
        .unwrap();
        let resolution = Resolution {
            entries: vec![ResolvedEntry {
                name: "tinytwo".into(),
                kind: EntryKind::Package,
                source: ResolvedSource::Archive {
                    source: "url::https://example.test/tinytwo.zip".into(),
                    url: format!("file://{}", archive_path.display()),
                    reference: None,
                },
                exclude: Vec::new(),
                include_tests: None,
                declared_sha256: Some(sha256),
                expected_tree_digest: None,
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
        let tree = project.path().join("deps-src/tinytwo");
        assert!(tree.join("DESCRIPTION").is_file());
        assert!(tree.join("R/hello.R").is_file());
        assert!(tree.join("man/hello.Rd").is_file());
        assert!(!tree.join("data").exists());
        assert!(!tree.join("tests").exists());
        assert_eq!(result.entries[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(
            result.entries[0].license.as_deref(),
            Some("Apache License (>= 2)")
        );
        assert_eq!(result.entries[0].fetch_method, FetchMethod::Tarball);
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
