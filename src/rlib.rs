//! Read-only R installation and library introspection.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use serde::Serialize;
use xshell::{Shell, cmd};

use crate::lock::Lockfile;

const INSPECTION_SCRIPT: &str = r#"
cat(paste(R.version$major, R.version$minor, sep = "."), "\n", sep = "")
packages <- installed.packages()[, c("Package", "Version"), drop = FALSE]
write.table(packages, stdout(), sep = "\t", row.names = FALSE,
            col.names = FALSE, quote = FALSE)
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    Absent,
    Unavailable {
        reason: String,
    },
    Available {
        r_version: String,
        packages: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct CoherenceReport {
    pub status: CoherenceStatus,
    pub r_version: Option<String>,
    pub note: Option<String>,
    pub mismatches: Vec<CoherenceMismatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoherenceStatus {
    Clean,
    Mismatch,
    Skipped,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoherenceMismatch {
    pub package: String,
    pub vendored_version: String,
    pub installed_version: Option<String>,
}

impl CoherenceReport {
    #[must_use]
    pub fn has_mismatches(&self) -> bool {
        !self.mismatches.is_empty()
    }
}

#[must_use]
pub fn inspect() -> Inspection {
    let Some(path) = find_on_path("Rscript") else {
        return Inspection::Absent;
    };
    inspect_executable(&path)
}

#[must_use]
pub fn inspect_executable(executable: &Path) -> Inspection {
    let Ok(shell) = Shell::new() else {
        return Inspection::Unavailable {
            reason: "could not prepare Rscript command".into(),
        };
    };
    let output = match cmd!(shell, "{executable} --vanilla -e {INSPECTION_SCRIPT}")
        .quiet()
        .read()
    {
        Ok(output) => output,
        Err(error) => {
            return Inspection::Unavailable {
                reason: format!("Rscript inspection failed: {error}"),
            };
        }
    };
    parse_inspection(&output).unwrap_or_else(|reason| Inspection::Unavailable { reason })
}

#[must_use]
pub fn check_coherence(lock: &Lockfile, inspection: &Inspection) -> CoherenceReport {
    match inspection {
        Inspection::Absent => CoherenceReport {
            status: CoherenceStatus::Skipped,
            r_version: None,
            note: Some(
                "Rscript was not found on PATH; installed-library coherence was skipped".into(),
            ),
            mismatches: Vec::new(),
        },
        Inspection::Unavailable { reason } => CoherenceReport {
            status: CoherenceStatus::Unavailable,
            r_version: None,
            note: Some(reason.clone()),
            mismatches: Vec::new(),
        },
        Inspection::Available {
            r_version,
            packages,
        } => {
            let mismatches = lock
                .packages
                .iter()
                .filter_map(|package| {
                    let installed = packages.get(&package.name);
                    (installed.map(String::as_str) != Some(package.version.as_str())).then(|| {
                        CoherenceMismatch {
                            package: package.name.clone(),
                            vendored_version: package.version.clone(),
                            installed_version: installed.cloned(),
                        }
                    })
                })
                .collect::<Vec<_>>();
            CoherenceReport {
                status: if mismatches.is_empty() {
                    CoherenceStatus::Clean
                } else {
                    CoherenceStatus::Mismatch
                },
                r_version: Some(r_version.clone()),
                note: None,
                mismatches,
            }
        }
    }
}

fn parse_inspection(output: &str) -> std::result::Result<Inspection, String> {
    let mut lines = output.lines();
    let version = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .ok_or_else(|| "Rscript inspection returned no R version".to_owned())?;
    let mut packages = BTreeMap::new();
    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (name, package_version) = line.split_once('\t').ok_or_else(|| {
            format!(
                "Rscript inspection returned malformed package data at output line {}",
                index + 2
            )
        })?;
        if name.is_empty() || package_version.is_empty() {
            return Err(format!(
                "Rscript inspection returned an empty package field at output line {}",
                index + 2
            ));
        }
        packages.insert(name.to_owned(), package_version.to_owned());
    }
    Ok(Inspection::Available {
        r_version: version.to_owned(),
        packages,
    })
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::tempdir;

    use super::{
        CoherenceStatus, Inspection, check_coherence, inspect_executable, parse_inspection,
    };
    use crate::lock::{FetchMethod, LockedPackage, Lockfile};

    #[test]
    fn parses_read_only_rscript_output() {
        let parsed = parse_inspection("4.5.1\nbase\t4.5.1\ntinyone\t1.0.0\n").unwrap();
        assert_eq!(
            parsed,
            Inspection::Available {
                r_version: "4.5.1".into(),
                packages: BTreeMap::from([
                    ("base".into(), "4.5.1".into()),
                    ("tinyone".into(), "1.0.0".into()),
                ]),
            }
        );
        assert!(parse_inspection("").is_err());
        assert!(parse_inspection("4.5.1\nmalformed\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fake_rscript_is_invoked_without_writing_a_library() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let executable = directory.path().join("Rscript");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '4.5.1\\ntinyone\\t9.9.9\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            inspect_executable(&executable),
            Inspection::Available {
                r_version: "4.5.1".into(),
                packages: BTreeMap::from([("tinyone".into(), "9.9.9".into())]),
            }
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn coherence_distinguishes_mismatch_from_skipped() {
        let lock = lock_with_package();
        let mismatch = check_coherence(
            &lock,
            &Inspection::Available {
                r_version: "4.5.1".into(),
                packages: BTreeMap::from([("tinyone".into(), "2.0.0".into())]),
            },
        );
        assert_eq!(mismatch.status, CoherenceStatus::Mismatch);
        assert_eq!(mismatch.mismatches[0].vendored_version, "1.0.0");
        assert_eq!(
            mismatch.mismatches[0].installed_version.as_deref(),
            Some("2.0.0")
        );

        let skipped = check_coherence(&lock, &Inspection::Absent);
        assert_eq!(skipped.status, CoherenceStatus::Skipped);
        assert!(!skipped.has_mismatches());
    }

    fn lock_with_package() -> Lockfile {
        Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "1970-01-01T00:00:00Z".into(),
            snapshot: None,
            config_hash: "sha256:config".into(),
            environment_digest: "sha256:environment".into(),
            packages: vec![LockedPackage {
                name: "tinyone".into(),
                version: "1.0.0".into(),
                source: "cran".into(),
                url: None,
                reference: None,
                commit: None,
                fetch_method: FetchMethod::Tarball,
                tarball_sha256: None,
                tree_digest: "sha256:tree".into(),
                files: BTreeMap::new(),
                license: Some("MIT".into()),
            }],
            references: Vec::new(),
        }
    }
}
