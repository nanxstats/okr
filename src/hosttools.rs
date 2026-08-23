//! Optional `git` and `gh` detection and shell-outs through `xshell`.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use xshell::{Shell, cmd};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct HostTools;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Availability {
    pub git: bool,
    pub gh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitRef {
    pub commit: String,
    pub matched_ref: String,
}

impl HostTools {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn availability(&self) -> Availability {
        Availability {
            git: command_on_path("git"),
            gh: command_on_path("gh"),
        }
    }

    #[must_use]
    pub fn git_available(&self) -> bool {
        command_on_path("git")
    }

    #[must_use]
    pub fn gh_available(&self) -> bool {
        command_on_path("gh")
    }

    #[must_use]
    pub fn gh_authenticated(&self) -> bool {
        if !self.gh_available() {
            return false;
        }
        let Ok(shell) = Shell::new() else {
            return false;
        };
        cmd!(shell, "gh auth status").quiet().output().is_ok()
    }

    pub fn git_ls_remote(&self, url: &str, reference: Option<&str>) -> Result<ResolvedGitRef> {
        self.require_git(
            "resolve a git ref (install git, or pin the source to a full 40-character commit SHA)",
        )?;
        let shell = Shell::new().map_err(shell_error)?;
        let output = if let Some(reference) = reference {
            let patterns = [
                reference.to_owned(),
                format!("refs/heads/{reference}"),
                format!("refs/tags/{reference}"),
                format!("refs/tags/{reference}^{{}}"),
            ];
            cmd!(shell, "git ls-remote --exit-code {url} {patterns...}")
                .quiet()
                .read()
        } else {
            cmd!(shell, "git ls-remote --exit-code --symref {url} HEAD")
                .quiet()
                .read()
        }
        .map_err(|error| {
            Error::Fetch(format!(
                "git ls-remote failed for `{url}`{}: {error}",
                reference.map_or_else(String::new, |value| format!(" at `{value}`"))
            ))
        })?;
        parse_ls_remote(&output, reference).ok_or_else(|| {
            Error::Fetch(format!(
                "git ls-remote returned no commit for `{url}`{}",
                reference.map_or_else(String::new, |value| format!(" at `{value}`"))
            ))
        })
    }

    pub fn gh_api_field(&self, endpoint: &str, field: &str) -> Result<String> {
        self.require_gh("query the GitHub API")?;
        let shell = Shell::new().map_err(shell_error)?;
        let output = cmd!(shell, "gh api {endpoint} --jq {field}")
            .quiet()
            .output()
            .map_err(|error| {
                Error::Fetch(format!(
                    "gh api failed for `{endpoint}`; check `gh auth status`: {error}"
                ))
            })?;
        let output = String::from_utf8(output.stdout).map_err(|error| {
            Error::Fetch(format!(
                "gh api returned non-UTF-8 output for `{endpoint}`: {error}"
            ))
        })?;
        let output = output.trim();
        if output.is_empty() {
            return Err(Error::Fetch(format!(
                "gh api returned an empty `{field}` for `{endpoint}`"
            )));
        }
        Ok(output.to_owned())
    }

    pub fn gh_api_bytes(&self, endpoint: &str) -> Result<Vec<u8>> {
        self.require_gh("download a private GitHub tarball")?;
        let shell = Shell::new().map_err(shell_error)?;
        let output = cmd!(shell, "gh api {endpoint}")
            .quiet()
            .output()
            .map_err(|error| {
                Error::Fetch(format!(
                    "gh api failed for `{endpoint}`; check `gh auth status`: {error}"
                ))
            })?;
        if output.stdout.is_empty() {
            return Err(Error::Fetch(format!(
                "gh api returned an empty response for `{endpoint}`"
            )));
        }
        Ok(output.stdout)
    }

    pub fn git_clone_at(
        &self,
        url: &str,
        reference: Option<&str>,
        commit: &str,
        destination: &Path,
    ) -> Result<()> {
        self.require_git("clone this source")?;
        let shell = Shell::new().map_err(shell_error)?;
        let mut branch_arguments = Vec::new();
        if let Some(reference) = reference
            && !crate::spec::is_full_commit_sha(reference)
        {
            branch_arguments.push("--branch".to_owned());
            branch_arguments.push(reference.to_owned());
            branch_arguments.push("--single-branch".to_owned());
        }
        cmd!(
            shell,
            "git -c core.autocrlf=false clone --quiet --depth 1 --no-checkout {branch_arguments...} {url} {destination}"
        )
        .run()
        .map_err(|error| {
            Error::Fetch(format!(
                "git clone failed for `{url}`; check the URL and your git credentials: {error}"
            ))
        })?;
        cmd!(shell, "git -C {destination} config core.autocrlf false")
            .quiet()
            .run()
            .map_err(|error| Error::Fetch(format!("could not configure clone: {error}")))?;

        let available = cmd!(shell, "git -C {destination} rev-parse HEAD")
            .quiet()
            .read()
            .map_err(|error| Error::Fetch(format!("could not inspect cloned HEAD: {error}")))?;
        if available.trim() != commit {
            cmd!(
                shell,
                "git -c core.autocrlf=false -C {destination} fetch --quiet --depth 1 origin {commit}"
            )
            .run()
            .map_err(|error| {
                Error::Fetch(format!(
                    "git could not fetch resolved commit {commit} from `{url}`: {error}"
                ))
            })?;
        }
        cmd!(
            shell,
            "git -c core.autocrlf=false -C {destination} checkout --quiet --detach {commit}"
        )
        .run()
        .map_err(|error| {
            Error::Fetch(format!(
                "git could not check out resolved commit {commit}: {error}"
            ))
        })?;
        let head = cmd!(shell, "git -C {destination} rev-parse HEAD")
            .quiet()
            .read()
            .map_err(|error| Error::Fetch(format!("could not verify cloned HEAD: {error}")))?;
        if head.trim() != commit {
            return Err(Error::Fetch(format!(
                "cloned HEAD mismatch for `{url}`: expected {commit}, found {}",
                head.trim()
            )));
        }
        Ok(())
    }

    fn require_git(&self, purpose: &str) -> Result<()> {
        if self.git_available() {
            Ok(())
        } else {
            Err(Error::Fetch(format!(
                "git is required to {purpose}, but `git` was not found on PATH"
            )))
        }
    }

    fn require_gh(&self, purpose: &str) -> Result<()> {
        if self.gh_available() {
            Ok(())
        } else {
            Err(Error::Fetch(format!(
                "GitHub CLI is required to {purpose}, but `gh` was not found on PATH; install gh and run `gh auth login`"
            )))
        }
    }
}

fn parse_ls_remote(output: &str, requested: Option<&str>) -> Option<ResolvedGitRef> {
    let mut candidates = Vec::new();
    for line in output.lines() {
        let Some((commit, reference)) = line.split_once('\t') else {
            continue;
        };
        if commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            candidates.push(ResolvedGitRef {
                commit: commit.to_ascii_lowercase(),
                matched_ref: reference.to_owned(),
            });
        }
    }
    if requested.is_none() {
        return candidates
            .into_iter()
            .find(|candidate| candidate.matched_ref == "HEAD");
    }
    candidates
        .iter()
        .find(|candidate| candidate.matched_ref.ends_with("^{}"))
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn command_on_path(program: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| command_on_explicit_path(program, &path))
}

fn command_on_explicit_path(program: &str, path: &OsStr) -> bool {
    env::split_paths(path).any(|directory| executable_file(&directory.join(program)))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn shell_error(error: xshell::Error) -> Error {
    Error::Fetch(format!("could not prepare host-tool command: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use xshell::{Shell, cmd};

    use super::{HostTools, command_on_explicit_path, parse_ls_remote};

    #[test]
    fn path_detection_requires_an_executable_file() {
        let directory = tempdir().unwrap();
        let tool = directory.path().join("tool");
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(command_on_explicit_path(
            "tool",
            directory.path().as_os_str()
        ));
        assert!(!command_on_explicit_path(
            "missing",
            directory.path().as_os_str()
        ));
    }

    #[test]
    fn ls_remote_parser_prefers_peeled_annotated_tags() {
        let tag_object = "1".repeat(40);
        let commit = "2".repeat(40);
        let output = format!("{tag_object}\trefs/tags/v1\n{commit}\trefs/tags/v1^{{}}\n");
        let resolved = parse_ls_remote(&output, Some("v1")).unwrap();
        assert_eq!(resolved.commit, commit);
        assert_eq!(resolved.matched_ref, "refs/tags/v1^{}");
    }

    #[test]
    fn git_ls_remote_works_with_a_file_url_fixture() {
        let tools = HostTools::new();
        if !tools.git_available() {
            return;
        }
        let directory = tempdir().unwrap();
        let repository = directory.path().join("fixture-repo");
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
        fs::write(repository.join("README.md"), "fixture\n").unwrap();
        cmd!(shell, "git -C {repository} add README.md")
            .run()
            .unwrap();
        cmd!(shell, "git -C {repository} commit -q -m fixture")
            .run()
            .unwrap();
        cmd!(shell, "git -C {repository} tag v1").run().unwrap();
        let expected = cmd!(shell, "git -C {repository} rev-parse HEAD")
            .read()
            .unwrap();
        let url = format!("file://{}", repository.display());

        assert_eq!(tools.git_ls_remote(&url, None).unwrap().commit, expected);
        assert_eq!(
            tools.git_ls_remote(&url, Some("main")).unwrap().commit,
            expected
        );
        assert_eq!(
            tools.git_ls_remote(&url, Some("v1")).unwrap().commit,
            expected
        );

        let clone_parent = tempdir().unwrap();
        let clone = clone_parent.path().join("clone");
        tools
            .git_clone_at(&url, Some("v1"), &expected, &clone)
            .unwrap();
        assert_eq!(
            cmd!(shell, "git -C {clone} config --get core.autocrlf")
                .read()
                .unwrap(),
            "false"
        );
        assert_eq!(
            cmd!(shell, "git -C {clone} rev-parse HEAD").read().unwrap(),
            expected
        );
    }
}
