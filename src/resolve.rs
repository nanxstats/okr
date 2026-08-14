//! Source resolution against snapshot and forge metadata.

pub mod dcf;

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::env;
use std::fs::File;
use std::io::Read;

use flate2::read::GzDecoder;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;

use crate::config::{Config, DeclaredEntry, DeclaredSource, EntryKind};
use crate::fetch::Fetcher;
use crate::hosttools::HostTools;
use crate::lock::{FetchMethod, LockedPackage, LockedReference, Lockfile};
use crate::progress::SyncProgress;
use crate::spec::{RemoteLocation, RemoteRef, RemoteSpec, RemoteType, is_full_commit_sha};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub entries: Vec<ResolvedEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntry {
    pub name: String,
    pub kind: EntryKind,
    pub source: ResolvedSource,
    pub exclude: Vec<String>,
    pub include_tests: Option<bool>,
    pub artifact_sha256: Option<String>,
    pub preferred_fetch_method: Option<FetchMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSource {
    Cran {
        version: String,
        url: String,
    },
    Tarball {
        source: String,
        url: String,
        reference: Option<String>,
    },
    Git {
        source: String,
        clone_url: String,
        requested_ref: Option<String>,
        locked_ref: Option<String>,
        commit: String,
        archive_url: Option<String>,
        github: Option<GithubRepository>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepository {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CranResolution {
    pub version: String,
    pub url: String,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotIndex {
    versions: BTreeMap<String, String>,
}

fn compare_package_versions(left: &str, right: &str) -> Ordering {
    let mut left = left.split(['.', '-']);
    let mut right = right.split(['.', '-']);

    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (left, right) => {
                let left = left.unwrap_or("0").trim_start_matches('0');
                let right = right.unwrap_or("0").trim_start_matches('0');
                let left = if left.is_empty() { "0" } else { left };
                let right = if right.is_empty() { "0" } else { right };
                let ordering = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

impl SnapshotIndex {
    pub fn load(fetcher: &Fetcher, repository: &str, snapshot: &str) -> Result<Self> {
        let url = format!(
            "{}/{snapshot}/src/contrib/PACKAGES.gz",
            repository.trim_end_matches('/')
        );
        let artifact = fetcher.fetch_url(&url, None, "snapshot PACKAGES index")?;
        let file = File::open(&artifact.path)?;
        let mut decoder = GzDecoder::new(file);
        let mut contents = String::new();
        decoder.read_to_string(&mut contents).map_err(|error| {
            Error::Fetch(format!(
                "could not decompress PACKAGES index from {url}: {error}"
            ))
        })?;
        let records = dcf::parse(&contents).map_err(|error| {
            Error::Fetch(format!(
                "could not parse PACKAGES index from {url}: {error}"
            ))
        })?;
        let mut versions = BTreeMap::new();
        for record in records {
            let name = record.get("Package").ok_or_else(|| {
                Error::Fetch(format!(
                    "PACKAGES index from {url} has a stanza without `Package`"
                ))
            })?;
            let version = record.get("Version").ok_or_else(|| {
                Error::Fetch(format!(
                    "PACKAGES index from {url} has no `Version` for `{name}`"
                ))
            })?;
            match versions.entry(name.to_owned()) {
                Entry::Vacant(entry) => {
                    entry.insert(version.to_owned());
                }
                Entry::Occupied(mut entry) => {
                    if compare_package_versions(version, entry.get()) == Ordering::Greater {
                        entry.insert(version.to_owned());
                    }
                }
            }
        }
        Ok(Self { versions })
    }

    pub fn resolve(
        &self,
        repository: &str,
        snapshot: &str,
        name: &str,
        requested_version: Option<&str>,
    ) -> Result<CranResolution> {
        let repository = repository.trim_end_matches('/');
        match requested_version {
            None => {
                let version = self.versions.get(name).ok_or_else(|| {
                    Error::Fetch(format!(
                        "package `{name}` is not present in snapshot {snapshot}"
                    ))
                })?;
                Ok(CranResolution {
                    version: version.clone(),
                    url: format!("{repository}/{snapshot}/src/contrib/{name}_{version}.tar.gz"),
                    archived: false,
                })
            }
            Some(version)
                if self
                    .versions
                    .get(name)
                    .is_some_and(|found| found == version) =>
            {
                Ok(CranResolution {
                    version: version.to_owned(),
                    url: format!("{repository}/{snapshot}/src/contrib/{name}_{version}.tar.gz"),
                    archived: false,
                })
            }
            Some(version) => Ok(CranResolution {
                version: version.to_owned(),
                url: format!(
                    "{repository}/{snapshot}/src/contrib/Archive/{name}/{name}_{version}.tar.gz"
                ),
                archived: true,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubApiMethod {
    Gh,
    Token,
    Anonymous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRelease {
    pub tag: String,
    pub commit: String,
    pub method: GithubApiMethod,
}

pub trait GithubReleaseApi {
    fn latest_release(&self, owner: &str, repo: &str) -> Result<GithubRelease>;
}

pub struct TieredGithubApi<'a> {
    tools: &'a HostTools,
    client: Client,
}

impl<'a> TieredGithubApi<'a> {
    pub fn new(tools: &'a HostTools) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("okr/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                Error::Fetch(format!("could not initialize GitHub client: {error}"))
            })?;
        Ok(Self { tools, client })
    }

    fn via_gh(&self, owner: &str, repo: &str) -> Result<GithubRelease> {
        let release_endpoint = format!("repos/{owner}/{repo}/releases/latest");
        let tag = self.tools.gh_api_field(&release_endpoint, ".tag_name")?;
        let commit_endpoint = format!("repos/{owner}/{repo}/commits/{}", encode_path_segment(&tag));
        let commit = self.tools.gh_api_field(&commit_endpoint, ".sha")?;
        validate_api_commit(&commit)?;
        Ok(GithubRelease {
            tag,
            commit: commit.to_ascii_lowercase(),
            method: GithubApiMethod::Gh,
        })
    }

    fn via_http(&self, owner: &str, repo: &str, token: Option<&str>) -> Result<GithubRelease> {
        let host = configured_github_host();
        let base = if host == "github.com" {
            "https://api.github.com".to_owned()
        } else {
            format!("https://{host}/api/v3")
        };
        let release: ReleaseResponse = self.get_json(
            &format!("{base}/repos/{owner}/{repo}/releases/latest"),
            token,
        )?;
        let commit: CommitResponse = self.get_json(
            &format!(
                "{base}/repos/{owner}/{repo}/commits/{}",
                encode_path_segment(&release.tag_name)
            ),
            token,
        )?;
        validate_api_commit(&commit.sha)?;
        Ok(GithubRelease {
            tag: release.tag_name,
            commit: commit.sha.to_ascii_lowercase(),
            method: if token.is_some() {
                GithubApiMethod::Token
            } else {
                GithubApiMethod::Anonymous
            },
        })
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str, token: Option<&str>) -> Result<T> {
        let mut request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(|error| {
            Error::Fetch(format!("GitHub API request failed for {url}: {error}"))
        })?;
        if response.status() == StatusCode::FORBIDDEN {
            return Err(Error::Fetch(
                "GitHub API returned HTTP 403; install `gh` and run `gh auth login`, or set GITHUB_TOKEN"
                    .into(),
            ));
        }
        if !response.status().is_success() {
            return Err(Error::Fetch(format!(
                "GitHub API request failed for {url}: HTTP {}",
                response.status()
            )));
        }
        response.json().map_err(|error| {
            Error::Fetch(format!("invalid GitHub API response from {url}: {error}"))
        })
    }
}

impl GithubReleaseApi for TieredGithubApi<'_> {
    fn latest_release(&self, owner: &str, repo: &str) -> Result<GithubRelease> {
        let mut failures = Vec::new();
        if self.tools.gh_authenticated() {
            match self.via_gh(owner, repo) {
                Ok(release) => return Ok(release),
                Err(error) => failures.push(format!("gh: {error}")),
            }
        }
        if let Some(token) = env::var("GITHUB_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
        {
            match self.via_http(owner, repo, Some(&token)) {
                Ok(release) => return Ok(release),
                Err(error) => failures.push(format!("token REST: {error}")),
            }
        }
        self.via_http(owner, repo, None).map_err(|error| {
            if failures.is_empty() {
                error
            } else {
                failures.push(format!("anonymous REST: {error}"));
                Error::Fetch(format!(
                    "GitHub release lookup exhausted all available tiers: {}",
                    failures.join("; ")
                ))
            }
        })
    }
}

#[derive(serde::Deserialize)]
struct ReleaseResponse {
    tag_name: String,
}

#[derive(serde::Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(serde::Deserialize)]
struct GitlabCommitResponse {
    id: String,
}

#[derive(serde::Deserialize)]
struct BitbucketCommitResponse {
    hash: String,
}

/// Resolve all declared entries without performing dependency solving.
pub fn resolve(
    config: &Config,
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &dyn GithubReleaseApi,
    previous: Option<&Lockfile>,
) -> Result<Resolution> {
    resolve_inner(config, fetcher, tools, github, previous, None)
}

pub(crate) fn resolve_with_progress(
    config: &Config,
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &dyn GithubReleaseApi,
    previous: Option<&Lockfile>,
    progress: &SyncProgress,
) -> Result<Resolution> {
    resolve_inner(config, fetcher, tools, github, previous, Some(progress))
}

fn resolve_inner(
    config: &Config,
    fetcher: &Fetcher,
    tools: &HostTools,
    github: &dyn GithubReleaseApi,
    previous: Option<&Lockfile>,
    progress: Option<&SyncProgress>,
) -> Result<Resolution> {
    let declarations = config.declared_entries()?;
    let mut entries = Vec::with_capacity(declarations.len());
    let mut warnings = Vec::new();
    let mut snapshot_index = None;

    if let Some(progress) = progress {
        progress.set_phase("Resolving sources...");
    }
    for declaration in declarations {
        let entry_progress =
            progress.map(|progress| progress.entry("Resolving", &declaration.name));
        let resolved = match &declaration.source {
            DeclaredSource::Cran { requested_version } => resolve_cran_entry(
                config,
                fetcher,
                previous,
                &declaration,
                requested_version,
                &mut snapshot_index,
            )?,
            DeclaredSource::Remote {
                spec,
                expected_sha256,
            } => resolve_remote_entry(
                fetcher,
                tools,
                github,
                previous,
                &declaration,
                spec,
                expected_sha256.as_deref(),
                &mut warnings,
            )?,
        };
        if let Some(entry_progress) = entry_progress {
            entry_progress.finish();
        }
        if let Some(progress) = progress {
            progress.advance(format!("Resolved {}", resolved.name));
        }
        entries.push(resolved);
    }

    Ok(Resolution { entries, warnings })
}

fn resolve_cran_entry(
    config: &Config,
    fetcher: &Fetcher,
    previous: Option<&Lockfile>,
    declaration: &DeclaredEntry,
    requested_version: &Option<String>,
    snapshot_index: &mut Option<SnapshotIndex>,
) -> Result<ResolvedEntry> {
    if fetcher.is_offline()
        && let Some(prior) = previous_package(previous, &declaration.name)
        && prior.source == "cran"
        && requested_version
            .as_ref()
            .is_none_or(|requested| requested == &prior.version)
        && let Some(url) = &prior.url
    {
        return Ok(ResolvedEntry {
            name: declaration.name.clone(),
            kind: declaration.kind,
            source: ResolvedSource::Cran {
                version: prior.version.clone(),
                url: url.clone(),
            },
            exclude: declaration.exclude.clone(),
            include_tests: declaration.include_tests,
            artifact_sha256: Some(locked_artifact_sha256(&prior.artifact_digest).to_owned()),
            preferred_fetch_method: Some(prior.fetch_method),
        });
    }

    let snapshot =
        config.project.snapshot.as_deref().ok_or_else(|| {
            Error::Config("project.snapshot is required for CRAN resolution".into())
        })?;
    if snapshot_index.is_none() {
        *snapshot_index = Some(SnapshotIndex::load(
            fetcher,
            config.repository_url(),
            snapshot,
        )?);
    }
    let cran = snapshot_index
        .as_ref()
        .expect("snapshot index was loaded")
        .resolve(
            config.repository_url(),
            snapshot,
            &declaration.name,
            requested_version.as_deref(),
        )?;
    let prior = previous_package(previous, &declaration.name)
        .filter(|prior| prior.source == "cran" && prior.version == cran.version);
    Ok(ResolvedEntry {
        name: declaration.name.clone(),
        kind: declaration.kind,
        source: ResolvedSource::Cran {
            version: cran.version,
            url: cran.url,
        },
        exclude: declaration.exclude.clone(),
        include_tests: declaration.include_tests,
        artifact_sha256: prior
            .map(|entry| locked_artifact_sha256(&entry.artifact_digest).to_owned()),
        preferred_fetch_method: prior.map(|entry| entry.fetch_method),
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_remote_entry(
    fetcher: &Fetcher,
    tools: &HostTools,
    github_api: &dyn GithubReleaseApi,
    previous: Option<&Lockfile>,
    declaration: &DeclaredEntry,
    spec: &RemoteSpec,
    expected_sha256: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<ResolvedEntry> {
    let source_id = source_id(spec);
    if let RemoteLocation::Url { url } = &spec.location {
        let prior = previous_remote(previous, declaration.kind, &declaration.name)
            .filter(|prior| prior.source() == source_id);
        return Ok(ResolvedEntry {
            name: declaration.name.clone(),
            kind: declaration.kind,
            source: ResolvedSource::Tarball {
                source: source_id,
                url: url.clone(),
                reference: spec.ref_name().map(str::to_owned),
            },
            exclude: declaration.exclude.clone(),
            include_tests: declaration.include_tests,
            artifact_sha256: Some(expected_sha256.expect("validated URL digest").to_owned()),
            preferred_fetch_method: prior.map(PriorRemote::fetch_method),
        });
    }

    let (clone_url, forge_coordinates) = git_location(spec)?;
    let prior = previous_remote(previous, declaration.kind, &declaration.name)
        .filter(|prior| prior.source() == source_id);
    let requested_ref = spec.ref_name().map(str::to_owned);
    let (commit, locked_ref, warn_named_ref) = match &spec.reference {
        Some(RemoteRef::Named(reference)) if is_full_commit_sha(reference) => (
            reference.to_ascii_lowercase(),
            Some(reference.clone()),
            false,
        ),
        _ if fetcher.is_offline() => {
            let prior = prior.ok_or_else(|| {
                Error::Fetch(format!(
                    "offline mode: `{}` needs a previously locked commit; run `okr sync` online first",
                    declaration.name
                ))
            })?;
            let commit = prior.commit().ok_or_else(|| {
                Error::Fetch(format!(
                    "offline mode: the prior lock has no commit for `{}`",
                    declaration.name
                ))
            })?;
            (
                commit.to_owned(),
                prior.reference().map(str::to_owned),
                false,
            )
        }
        Some(RemoteRef::LatestRelease) => {
            let RemoteLocation::Forge { owner, repo } = &spec.location else {
                return Err(Error::Spec(
                    "@*release is supported only for GitHub forge specifications".into(),
                ));
            };
            let release = github_api.latest_release(owner, repo)?;
            (release.commit, Some(release.tag), false)
        }
        Some(RemoteRef::Named(reference)) => {
            match tools.git_ls_remote(&clone_url, Some(reference)) {
                Ok(resolved) => {
                    let branch = resolved.matched_ref.starts_with("refs/heads/");
                    (resolved.commit, Some(reference.clone()), branch)
                }
                Err(git_error) => {
                    let commit = resolve_public_forge_ref(fetcher, spec, reference).map_err(
                        |api_error| {
                            Error::Fetch(format!(
                                "could not resolve {} at {reference}: git ls-remote failed ({git_error}); public forge API fallback failed ({api_error})",
                                declaration.name
                            ))
                        },
                    )?;
                    (commit, Some(reference.clone()), true)
                }
            }
        }
        None => match tools.git_ls_remote(&clone_url, None) {
            Ok(resolved) => (resolved.commit, None, true),
            Err(git_error) => {
                let commit = resolve_public_forge_ref(fetcher, spec, "HEAD").map_err(
                        |api_error| {
                            Error::Fetch(format!(
                                "could not resolve the default branch for {}: git ls-remote failed ({git_error}); public forge API fallback failed ({api_error})",
                                declaration.name
                            ))
                        },
                    )?;
                (commit, None, true)
            }
        },
    };

    if spec.reference.is_none() {
        warnings.push(format!(
            "{} has no ref; the default branch resolved to {} and that SHA will be locked",
            declaration.name, commit
        ));
    } else if warn_named_ref {
        warnings.push(format!(
            "{} uses ref {}; it resolved to {} and that SHA will be locked",
            declaration.name,
            requested_ref.as_deref().unwrap_or("HEAD"),
            commit
        ));
    }

    let (archive_url, github) = archive_for(&clone_url, forge_coordinates.as_ref(), &commit);
    let matching_prior = prior.filter(|prior| prior.commit() == Some(commit.as_str()));
    Ok(ResolvedEntry {
        name: declaration.name.clone(),
        kind: declaration.kind,
        source: ResolvedSource::Git {
            source: source_id,
            clone_url,
            requested_ref,
            locked_ref,
            commit,
            archive_url,
            github,
        },
        exclude: declaration.exclude.clone(),
        include_tests: declaration.include_tests,
        artifact_sha256: matching_prior
            .map(PriorRemote::artifact_sha256)
            .map(str::to_owned),
        preferred_fetch_method: matching_prior.map(PriorRemote::fetch_method),
    })
}

fn resolve_public_forge_ref(
    fetcher: &Fetcher,
    spec: &RemoteSpec,
    reference: &str,
) -> Result<String> {
    let RemoteLocation::Forge { owner, repo } = &spec.location else {
        return Err(Error::Fetch(
            "this source requires git; install `git` or pin and cache it before using --offline"
                .into(),
        ));
    };
    let encoded_owner = encode_path_segment(owner);
    let encoded_repo = encode_path_segment(repo);
    let encoded_ref = encode_path_segment(reference);
    let commit = match spec.remote_type {
        RemoteType::Github => {
            let host = configured_github_host();
            if host != "github.com" {
                return Err(Error::Fetch(format!(
                    "GitHub Enterprise source on {host} requires git or authenticated GitHub access"
                )));
            }
            let response: CommitResponse = fetcher.get_json(
                &format!(
                    "https://api.github.com/repos/{encoded_owner}/{encoded_repo}/commits/{encoded_ref}"
                ),
                "GitHub commit metadata",
            )?;
            response.sha
        }
        RemoteType::Gitlab => {
            let project = encode_path_segment(&format!("{owner}/{repo}"));
            let response: GitlabCommitResponse = fetcher.get_json(
                &format!(
                    "https://gitlab.com/api/v4/projects/{project}/repository/commits/{encoded_ref}"
                ),
                "GitLab commit metadata",
            )?;
            response.id
        }
        RemoteType::Bitbucket => {
            let response: BitbucketCommitResponse = fetcher.get_json(
                &format!(
                    "https://api.bitbucket.org/2.0/repositories/{encoded_owner}/{encoded_repo}/commit/{encoded_ref}"
                ),
                "Bitbucket commit metadata",
            )?;
            response.hash
        }
        RemoteType::Git | RemoteType::Url => {
            return Err(Error::Fetch(
                "this source requires git; install `git` or pin and cache it before using --offline"
                    .into(),
            ));
        }
    };
    validate_api_commit(&commit)?;
    Ok(commit.to_ascii_lowercase())
}

fn source_id(spec: &RemoteSpec) -> String {
    match &spec.location {
        RemoteLocation::Forge { owner, repo } => {
            format!("{}::{owner}/{repo}", spec.remote_type)
        }
        RemoteLocation::Git { url } | RemoteLocation::Url { url } => {
            format!("{}::{url}", spec.remote_type)
        }
    }
}

fn git_location(spec: &RemoteSpec) -> Result<(String, Option<GithubRepository>)> {
    match (&spec.remote_type, &spec.location) {
        (RemoteType::Github, RemoteLocation::Forge { owner, repo }) => Ok((
            format!("https://{}/{owner}/{repo}.git", configured_github_host()),
            Some(GithubRepository {
                host: configured_github_host(),
                owner: owner.clone(),
                repo: repo.clone(),
            }),
        )),
        (RemoteType::Gitlab, RemoteLocation::Forge { owner, repo }) => {
            Ok((format!("https://gitlab.com/{owner}/{repo}.git"), None))
        }
        (RemoteType::Bitbucket, RemoteLocation::Forge { owner, repo }) => {
            Ok((format!("https://bitbucket.org/{owner}/{repo}.git"), None))
        }
        (RemoteType::Git, RemoteLocation::Git { url }) => Ok((url.clone(), None)),
        _ => Err(Error::Spec(format!(
            "{} is not a git source",
            source_id(spec)
        ))),
    }
}

fn archive_for(
    clone_url: &str,
    known_github: Option<&GithubRepository>,
    commit: &str,
) -> (Option<String>, Option<GithubRepository>) {
    if let Some(github) = known_github {
        if github.host != "github.com" {
            return (None, Some(github.clone()));
        }
        return (
            Some(format!(
                "https://{}/{}/{}/archive/{commit}.tar.gz",
                github.host, github.owner, github.repo
            )),
            Some(github.clone()),
        );
    }
    let Some((host, repository_path)) = split_git_host_path(clone_url) else {
        return (None, None);
    };
    let repository_path = repository_path.trim_end_matches(".git");
    let repo = repository_path
        .rsplit('/')
        .next()
        .unwrap_or(repository_path);
    match host.as_str() {
        "github.com" => {
            let Some((owner, repo)) = repository_path.split_once('/') else {
                return (None, None);
            };
            let github = GithubRepository {
                host: "github.com".into(),
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            };
            (
                Some(format!(
                    "https://github.com/{repository_path}/archive/{commit}.tar.gz"
                )),
                Some(github),
            )
        }
        "gitlab.com" => (
            Some(format!(
                "https://gitlab.com/{repository_path}/-/archive/{commit}/{repo}-{commit}.tar.gz"
            )),
            None,
        ),
        "bitbucket.org" => (
            Some(format!(
                "https://bitbucket.org/{repository_path}/get/{commit}.tar.gz"
            )),
            None,
        ),
        "codeberg.org" => (
            Some(format!(
                "https://codeberg.org/{repository_path}/archive/{commit}.tar.gz"
            )),
            None,
        ),
        _ => (None, None),
    }
}

fn configured_github_host() -> String {
    env::var("GH_HOST")
        .ok()
        .filter(|host| !host.is_empty())
        .map(|host| {
            host.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_owned()
        })
        .unwrap_or_else(|| "github.com".into())
}

fn split_git_host_path(url: &str) -> Option<(String, String)> {
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (
            authority.rsplit('@').next().unwrap_or(authority),
            path.to_owned(),
        )
    } else {
        let (authority, path) = url.split_once(':')?;
        (
            authority.rsplit('@').next().unwrap_or(authority),
            path.to_owned(),
        )
    };
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_matches('/').to_owned();
    if host.is_empty() || path.is_empty() {
        None
    } else {
        Some((host, path))
    }
}

#[derive(Clone, Copy)]
enum PriorRemote<'a> {
    Package(&'a LockedPackage),
    Reference(&'a LockedReference),
}

impl<'a> PriorRemote<'a> {
    fn source(self) -> &'a str {
        match self {
            Self::Package(entry) => &entry.source,
            Self::Reference(entry) => &entry.source,
        }
    }

    fn reference(self) -> Option<&'a str> {
        match self {
            Self::Package(entry) => entry.reference.as_deref(),
            Self::Reference(entry) => entry.reference.as_deref(),
        }
    }

    fn commit(self) -> Option<&'a str> {
        match self {
            Self::Package(entry) => entry.commit.as_deref(),
            Self::Reference(entry) => entry.commit.as_deref(),
        }
    }

    const fn fetch_method(self) -> FetchMethod {
        match self {
            Self::Package(entry) => entry.fetch_method,
            Self::Reference(entry) => entry.fetch_method,
        }
    }

    fn artifact_sha256(self) -> &'a str {
        match self {
            Self::Package(entry) => locked_artifact_sha256(&entry.artifact_digest),
            Self::Reference(entry) => locked_artifact_sha256(&entry.artifact_digest),
        }
    }
}

fn locked_artifact_sha256(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

fn previous_package<'a>(lock: Option<&'a Lockfile>, name: &str) -> Option<&'a LockedPackage> {
    lock?.packages.iter().find(|entry| entry.name == name)
}

fn previous_remote<'a>(
    lock: Option<&'a Lockfile>,
    kind: EntryKind,
    name: &str,
) -> Option<PriorRemote<'a>> {
    let lock = lock?;
    match kind {
        EntryKind::Package => lock
            .packages
            .iter()
            .find(|entry| entry.name == name)
            .map(PriorRemote::Package),
        EntryKind::Reference => lock
            .references
            .iter()
            .find(|entry| entry.name == name)
            .map(PriorRemote::Reference),
    }
}

fn encode_path_segment(value: &str) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            write!(&mut output, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    output
}

fn validate_api_commit(commit: &str) -> Result<()> {
    if is_full_commit_sha(commit) {
        Ok(())
    } else {
        Err(Error::Fetch(format!(
            "GitHub API returned invalid commit SHA `{commit}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;

    use super::{
        GithubApiMethod, GithubRelease, GithubReleaseApi, GithubRepository, ResolvedSource,
        SnapshotIndex, archive_for, compare_package_versions, encode_path_segment, resolve,
        resolve_public_forge_ref, split_git_host_path,
    };
    use crate::config::Config;
    use crate::fetch::{Cache, Fetcher};
    use crate::hosttools::HostTools;
    use crate::lock::{FetchMethod, LockedReference, Lockfile};

    struct StubGithub;

    impl GithubReleaseApi for StubGithub {
        fn latest_release(&self, owner: &str, repo: &str) -> crate::Result<GithubRelease> {
            assert_eq!((owner, repo), ("owner", "repo"));
            Ok(GithubRelease {
                tag: "v2.0.0".into(),
                commit: "a".repeat(40),
                method: GithubApiMethod::Anonymous,
            })
        }
    }

    #[test]
    fn snapshot_resolution_is_lookup_and_archive_fallback_only() {
        let root = tempdir().unwrap();
        let contrib = root.path().join("2026-06-30/src/contrib");
        fs::create_dir_all(&contrib).unwrap();
        let file = fs::File::create(contrib.join("PACKAGES.gz")).unwrap();
        let mut gzip = GzEncoder::new(file, Compression::default());
        gzip.write_all(b"Package: alpha\nVersion: 2.0.0\n\nPackage: beta\nVersion: 1.5.0\n")
            .unwrap();
        gzip.finish().unwrap();
        let repository = format!("file://{}", root.path().display());
        let cache = Cache::new(root.path().join("cache"));
        let fetcher = Fetcher::new(cache, false).unwrap();
        let index = SnapshotIndex::load(&fetcher, &repository, "2026-06-30").unwrap();

        let latest = index
            .resolve(&repository, "2026-06-30", "alpha", None)
            .unwrap();
        assert_eq!(latest.version, "2.0.0");
        assert!(!latest.archived);

        let current = index
            .resolve(&repository, "2026-06-30", "alpha", Some("2.0.0"))
            .unwrap();
        assert!(!current.archived);

        let old = index
            .resolve(&repository, "2026-06-30", "alpha", Some("1.0.0"))
            .unwrap();
        assert!(old.archived);
        assert!(old.url.ends_with("/Archive/alpha/alpha_1.0.0.tar.gz"));

        assert!(
            index
                .resolve(&repository, "2026-06-30", "missing", None)
                .is_err()
        );
        let archived_missing = index
            .resolve(&repository, "2026-06-30", "missing", Some("0.1.0"))
            .unwrap();
        assert!(archived_missing.archived);
    }

    #[test]
    fn snapshot_index_accepts_same_version_recommended_package_duplicates() {
        let root = tempdir().unwrap();
        let contrib = root.path().join("2026-06-30/src/contrib");
        fs::create_dir_all(&contrib).unwrap();
        let file = fs::File::create(contrib.join("PACKAGES.gz")).unwrap();
        let mut gzip = GzEncoder::new(file, Compression::default());
        gzip.write_all(
            b"Package: boot\nVersion: 1.3-32\nPath: 4.7.0/Recommended\nPriority: recommended\n\nPackage: boot\nVersion: 1.3-32\nPriority: recommended\n",
        )
        .unwrap();
        gzip.finish().unwrap();
        let repository = format!("file://{}", root.path().display());
        let fetcher = Fetcher::new(Cache::new(root.path().join("cache")), false).unwrap();

        let index = SnapshotIndex::load(&fetcher, &repository, "2026-06-30").unwrap();
        let resolved = index
            .resolve(&repository, "2026-06-30", "boot", None)
            .unwrap();

        assert_eq!(resolved.version, "1.3-32");
        assert_eq!(
            resolved.url,
            format!("{repository}/2026-06-30/src/contrib/boot_1.3-32.tar.gz")
        );
    }

    #[test]
    fn snapshot_index_chooses_greatest_conflicting_duplicate_version() {
        let root = tempdir().unwrap();
        let contrib = root.path().join("2026-06-30/src/contrib");
        fs::create_dir_all(&contrib).unwrap();
        let file = fs::File::create(contrib.join("PACKAGES.gz")).unwrap();
        let mut gzip = GzEncoder::new(file, Compression::default());
        gzip.write_all(
            b"Package: cluster\nVersion: 2.1.8.2\nPath: 4.7.0/Recommended\nPriority: recommended\n\nPackage: cluster\nVersion: 2.1.8.3\nPriority: recommended\n",
        )
        .unwrap();
        gzip.finish().unwrap();
        let repository = format!("file://{}", root.path().display());
        let fetcher = Fetcher::new(Cache::new(root.path().join("cache")), false).unwrap();

        let index = SnapshotIndex::load(&fetcher, &repository, "2026-06-30").unwrap();
        let resolved = index
            .resolve(&repository, "2026-06-30", "cluster", None)
            .unwrap();

        assert_eq!(resolved.version, "2.1.8.3");
        assert_eq!(
            resolved.url,
            format!("{repository}/2026-06-30/src/contrib/cluster_2.1.8.3.tar.gz")
        );
    }

    #[test]
    fn package_version_comparison_is_numeric_and_order_independent() {
        use std::cmp::Ordering;

        assert_eq!(compare_package_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_package_versions("1.9", "1.10"), Ordering::Less);
        assert_eq!(compare_package_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_package_versions("1-02", "1.2"), Ordering::Equal);
    }

    #[test]
    fn latest_release_resolution_is_stubbable_and_freezes_the_sha() {
        let config = Config::parse("[packages]\npkg = \"owner/repo@*release\"").unwrap();
        let cache = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(cache.path()), false).unwrap();
        let resolution = resolve(&config, &fetcher, &HostTools::new(), &StubGithub, None).unwrap();
        match &resolution.entries[0].source {
            ResolvedSource::Git {
                commit, locked_ref, ..
            } => {
                assert_eq!(commit, &"a".repeat(40));
                assert_eq!(locked_ref.as_deref(), Some("v2.0.0"));
            }
            source => panic!("unexpected source: {source:?}"),
        }
    }

    #[test]
    fn full_commit_pins_skip_git_resolution() {
        let sha = "b".repeat(40);
        let config = Config::parse(&format!("[packages]\npkg = \"owner/repo@{sha}\"")).unwrap();
        let cache = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(cache.path()), false).unwrap();
        let resolution = resolve(&config, &fetcher, &HostTools::new(), &StubGithub, None).unwrap();
        assert!(matches!(
            &resolution.entries[0].source,
            ResolvedSource::Git { commit, .. } if commit == &sha
        ));
    }

    #[test]
    fn offline_resolution_reuses_a_locked_git_commit() {
        let config =
            Config::parse("[references]\nstandards = \"git::file:///unavailable/repo@main\"")
                .unwrap();
        let lock = Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "2026-08-11T00:00:00Z".into(),
            snapshot: None,
            config_digest: "sha256:x".into(),
            environment_digest: "sha256:y".into(),
            packages: Vec::new(),
            references: vec![LockedReference {
                name: "standards".into(),
                source: "git::file:///unavailable/repo".into(),
                url: None,
                reference: Some("main".into()),
                commit: Some("c".repeat(40)),
                fetch_method: FetchMethod::GitClone,
                artifact_digest: format!("sha256:{}", "d".repeat(64)),
                tree_digest: format!("sha256:{}", "e".repeat(64)),
                license: None,
            }],
        };
        let cache = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(cache.path()), true).unwrap();
        let resolution = resolve(
            &config,
            &fetcher,
            &HostTools::new(),
            &StubGithub,
            Some(&lock),
        )
        .unwrap();
        assert!(matches!(
            &resolution.entries[0].source,
            ResolvedSource::Git { commit, .. } if commit == &"c".repeat(40)
        ));
        assert_eq!(
            resolution.entries[0].preferred_fetch_method,
            Some(FetchMethod::GitClone)
        );
    }

    #[test]
    fn recognized_forge_archive_urls_cover_the_fetch_table() {
        let sha = "f".repeat(40);
        let cases = [
            (
                "https://github.com/org/repo.git",
                "https://github.com/org/repo/archive/ffffffffffffffffffffffffffffffffffffffff.tar.gz",
            ),
            (
                "git@gitlab.com:org/repo.git",
                "https://gitlab.com/org/repo/-/archive/ffffffffffffffffffffffffffffffffffffffff/repo-ffffffffffffffffffffffffffffffffffffffff.tar.gz",
            ),
            (
                "ssh://git@bitbucket.org/org/repo.git",
                "https://bitbucket.org/org/repo/get/ffffffffffffffffffffffffffffffffffffffff.tar.gz",
            ),
            (
                "https://codeberg.org/org/repo.git",
                "https://codeberg.org/org/repo/archive/ffffffffffffffffffffffffffffffffffffffff.tar.gz",
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(archive_for(url, None, &sha).0.as_deref(), Some(expected));
        }
        assert_eq!(archive_for("file:///tmp/repo", None, &sha).0, None);
        let enterprise = GithubRepository {
            host: "github.corp.example".into(),
            owner: "org".into(),
            repo: "repo".into(),
        };
        let plan = archive_for(
            "https://github.corp.example/org/repo.git",
            Some(&enterprise),
            &sha,
        );
        assert_eq!(plan.0, None);
        assert_eq!(plan.1, Some(enterprise));
    }

    #[test]
    fn git_url_host_parsing_and_api_path_encoding_are_deterministic() {
        assert_eq!(
            split_git_host_path("git@github.com:org/repo.git"),
            Some(("github.com".into(), "org/repo.git".into()))
        );
        assert_eq!(
            split_git_host_path("ssh://git@gitlab.com/org/repo.git"),
            Some(("gitlab.com".into(), "org/repo.git".into()))
        );
        assert_eq!(encode_path_segment("release/1 +2"), "release%2F1%20%2B2");
    }

    #[test]
    fn public_forge_api_fallback_urls_are_tested_offline() {
        let directory = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(directory.path()), true).unwrap();
        let cases = [
            (
                "gitlab::org/repo@release/1",
                "https://gitlab.com/api/v4/projects/org%2Frepo/repository/commits/release%2F1",
            ),
            (
                "bitbucket::org/repo@release/1",
                "https://api.bitbucket.org/2.0/repositories/org/repo/commit/release%2F1",
            ),
        ];
        for (raw, expected_url) in cases {
            let spec = crate::spec::RemoteSpec::parse(raw).unwrap();
            let error = resolve_public_forge_ref(&fetcher, &spec, "release/1").unwrap_err();
            assert!(error.to_string().contains(expected_url), "{error}");
        }
    }
}
