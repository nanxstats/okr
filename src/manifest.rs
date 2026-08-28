//! Human- and machine-readable manifests and agent discovery affordances.

use std::fs;
use std::io::Write;
use std::path::{Component, Path};

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::config::{Config, EntryKind};
use crate::digest::TreeDigest;
use crate::lock::Lockfile;
use crate::resolve::dcf;
use crate::vendor::VendorResult;
use crate::{Error, Result};

pub const AGENTS_BEGIN: &str = "<!-- okr:begin -->";
pub const AGENTS_END: &str = "<!-- okr:end -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOutput {
    pub json: String,
    pub markdown: String,
}

#[derive(Debug, Serialize)]
struct JsonManifest {
    schema: u32,
    environment_digest: String,
    entries: Vec<JsonEntry>,
}

#[derive(Debug, Serialize)]
struct JsonEntry {
    kind: EntryKind,
    name: String,
    version: Option<String>,
    commit: Option<String>,
    source: String,
    license: Option<String>,
    path: String,
    title: Option<String>,
    tree_digest: String,
    artifact_digest: String,
}

pub fn render(config: &Config, lock: &Lockfile, vendored: &VendorResult) -> Result<ManifestOutput> {
    let vendor_path = normalized_config_path(&config.vendor.path)?;
    let mut entries = Vec::with_capacity(lock.packages.len() + lock.references.len());
    for package in &lock.packages {
        let title = vendored
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Package && entry.name == package.name)
            .and_then(|entry| entry.title.clone());
        entries.push(JsonEntry {
            kind: EntryKind::Package,
            name: package.name.clone(),
            version: Some(package.version.clone()),
            commit: package.commit.clone(),
            source: package.source.clone(),
            license: package.license.clone(),
            path: format!("{vendor_path}/{}", package.name),
            title,
            tree_digest: package.tree_digest.clone(),
            artifact_digest: package.artifact_digest.clone(),
        });
    }
    for reference in &lock.references {
        let title = vendored
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Reference && entry.name == reference.name)
            .and_then(|entry| entry.title.clone());
        entries.push(JsonEntry {
            kind: EntryKind::Reference,
            name: reference.name.clone(),
            version: None,
            commit: reference.commit.clone(),
            source: reference.source.clone(),
            license: reference.license.clone(),
            path: format!("{vendor_path}/{}", reference.name),
            title,
            tree_digest: reference.tree_digest.clone(),
            artifact_digest: reference.artifact_digest.clone(),
        });
    }
    entries.sort_by(|left, right| (left.kind, &left.name).cmp(&(right.kind, &right.name)));

    let manifest = JsonManifest {
        schema: 1,
        environment_digest: lock.environment_digest.clone(),
        entries,
    };
    let mut json = serde_json::to_string_pretty(&manifest)
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
    json.push('\n');
    let markdown = render_markdown(config, lock, vendored)?;
    Ok(ManifestOutput { json, markdown })
}

pub fn write_manifests(
    config: &Config,
    lock: &Lockfile,
    vendored: &VendorResult,
) -> Result<ManifestOutput> {
    let output = render(config, lock, vendored)?;
    fs::create_dir_all(&vendored.root)?;
    atomic_write(&vendored.root.join("_manifest.json"), &output.json)?;
    atomic_write(&vendored.root.join("_manifest.md"), &output.markdown)?;
    Ok(output)
}

/// Reconstruct the expected manifests from the lock and intact package
/// metadata so `verify` also covers the generated files at the vendor root.
pub(crate) fn render_for_verification(
    config: &Config,
    lock: &Lockfile,
    root: &Path,
) -> Result<ManifestOutput> {
    let mut entries = Vec::with_capacity(lock.packages.len() + lock.references.len());
    for package in &lock.packages {
        let description = fs::read_to_string(root.join(&package.name).join("DESCRIPTION"))?;
        let record = dcf::parse_one(&description).map_err(|error| {
            Error::Verification(format!(
                "cannot reconstruct manifest metadata for {}: {error}",
                package.name
            ))
        })?;
        entries.push(crate::vendor::VendoredEntry {
            name: package.name.clone(),
            kind: EntryKind::Package,
            version: Some(package.version.clone()),
            license: package.license.clone(),
            title: record
                .get("Title")
                .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" ")),
            fetch_method: package.fetch_method,
            artifact_sha256: package
                .artifact_digest
                .strip_prefix("sha256:")
                .unwrap_or(&package.artifact_digest)
                .to_owned(),
            tree: TreeDigest {
                digest: package.tree_digest.clone(),
                files: Default::default(),
            },
        });
    }
    for reference in &lock.references {
        let title = crate::vendor::reference_title(&root.join(&reference.name));
        entries.push(crate::vendor::VendoredEntry {
            name: reference.name.clone(),
            kind: EntryKind::Reference,
            version: None,
            license: reference.license.clone(),
            title,
            fetch_method: reference.fetch_method,
            artifact_sha256: reference
                .artifact_digest
                .strip_prefix("sha256:")
                .unwrap_or(&reference.artifact_digest)
                .to_owned(),
            tree: TreeDigest {
                digest: reference.tree_digest.clone(),
                files: Default::default(),
            },
        });
    }
    render(
        config,
        lock,
        &VendorResult {
            root: root.to_owned(),
            entries,
            warnings: Vec::new(),
        },
    )
}

pub fn update_agents_file(project_directory: &Path, config: &Config) -> Result<()> {
    if !config.manifest.agents_file {
        return Ok(());
    }
    let path = project_directory.join("AGENTS.md");
    let existing = if path.try_exists()? {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let vendor_path = normalized_config_path(&config.vendor.path)?;
    let block = agents_block(&vendor_path, newline);
    let updated = replace_or_append_marker_block(&existing, &block, newline)?;
    if updated != existing {
        atomic_write_preserving_permissions(&path, &updated)?;
    }
    Ok(())
}

pub fn update_gitignore(project_directory: &Path, config: &Config) -> Result<()> {
    if !config.vendor.gitignore {
        return Ok(());
    }
    let path = project_directory.join(".gitignore");
    let existing = if path.try_exists()? {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let vendor_path = normalized_config_path(&config.vendor.path)?;
    let entry = format!("/{vendor_path}/");
    let equivalent = format!("{vendor_path}/");
    if existing
        .lines()
        .map(str::trim)
        .any(|line| line == entry || line == equivalent)
    {
        return Ok(());
    }
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with(['\n', '\r']) {
        updated.push_str(newline);
    }
    updated.push_str(&entry);
    updated.push_str(newline);
    atomic_write_preserving_permissions(&path, &updated)
}

pub fn update_rbuildignore(project_directory: &Path, config: &Config) -> Result<()> {
    let path = project_directory.join(".Rbuildignore");
    if !path.try_exists()? {
        return Ok(());
    }

    let existing = fs::read_to_string(&path)?;
    let vendor_path = normalized_config_path(&config.vendor.path)?;
    let rules = [
        rbuildignore_rule(&vendor_path),
        rbuildignore_rule("okr.toml"),
        rbuildignore_rule("okr.lock"),
    ];
    let missing = rules
        .into_iter()
        .filter(|rule| !existing.lines().any(|line| line == rule))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }

    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with(['\n', '\r']) {
        updated.push_str(newline);
    }
    for rule in missing {
        updated.push_str(&rule);
        updated.push_str(newline);
    }
    atomic_write_preserving_permissions(&path, &updated)
}

fn render_markdown(config: &Config, lock: &Lockfile, vendored: &VendorResult) -> Result<String> {
    let vendor_path = normalized_config_path(&config.vendor.path)?;
    let mut markdown = format!(
        "# Vendored source manifest\n\nThis tree contains generated, read-only R dependency sources and reference repositories for coding agent context. Do not edit it directly; run `okr sync` to regenerate it.\n\nEnvironment digest: `{}`\n\n## Packages\n\n| Name | Version | Source | License | Path | Description |\n|---|---|---|---|---|---|\n",
        lock.environment_digest
    );
    for package in &lock.packages {
        let title = vendored
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Package && entry.name == package.name)
            .and_then(|entry| entry.title.as_deref())
            .unwrap_or("—");
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | `{vendor_path}/{}` | {} |\n",
            markdown_cell(&package.name),
            markdown_cell(&package.version),
            markdown_cell(&package.source),
            markdown_cell(package.license.as_deref().unwrap_or("—")),
            package.name,
            markdown_cell(title),
        ));
    }
    markdown.push_str(
        "\n## References\n\n| Name | Commit | Source | License | Path | Description |\n|---|---|---|---|---|---|\n",
    );
    for reference in &lock.references {
        let title = vendored
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Reference && entry.name == reference.name)
            .and_then(|entry| entry.title.as_deref())
            .unwrap_or("—");
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | `{vendor_path}/{}` | {} |\n",
            markdown_cell(&reference.name),
            markdown_cell(reference.commit.as_deref().unwrap_or("—")),
            markdown_cell(&reference.source),
            markdown_cell(reference.license.as_deref().unwrap_or("—")),
            reference.name,
            markdown_cell(title),
        ));
    }
    Ok(markdown)
}

fn markdown_cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

fn normalized_config_path(path: &Path) -> Result<String> {
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(Error::Config(format!(
                "vendor path is not normalized: {}",
                path.display()
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            Error::Config(format!("vendor path is not UTF-8: {}", path.display()))
        })?;
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(part);
    }
    Ok(output)
}

fn rbuildignore_rule(path: &str) -> String {
    let path = path.trim_end_matches('/');
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(
            character,
            '\\' | '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    format!("^{escaped}$")
}

fn agents_block(vendor_path: &str, newline: &str) -> String {
    [
        AGENTS_BEGIN.to_owned(),
        format!("Vendored R dependency sources and reference repos live in `{vendor_path}/`"),
        format!("(see `{vendor_path}/_manifest.md`). Read them to understand APIs and"),
        "internals. Do not edit them; they are generated by `okr sync` and".into(),
        "verified by hash.".into(),
        AGENTS_END.to_owned(),
    ]
    .join(newline)
}

fn replace_or_append_marker_block(existing: &str, block: &str, newline: &str) -> Result<String> {
    let begins = existing.match_indices(AGENTS_BEGIN).collect::<Vec<_>>();
    let ends = existing.match_indices(AGENTS_END).collect::<Vec<_>>();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => {
            let mut output = existing.to_owned();
            if !output.is_empty() && !output.ends_with(['\n', '\r']) {
                output.push_str(newline);
            }
            output.push_str(block);
            output.push_str(newline);
            Ok(output)
        }
        ([(begin, _)], [(end, _)]) if begin < end => {
            let end = end + AGENTS_END.len();
            Ok(format!("{}{}{}", &existing[..*begin], block, &existing[end..]))
        }
        _ => Err(Error::Config(
            "AGENTS.md has malformed or duplicate okr marker blocks; repair the <!-- okr:begin --> / <!-- okr:end --> pair"
                .into(),
        )),
    }
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn atomic_write_preserving_permissions(path: &Path, contents: &str) -> Result<()> {
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.flush()?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::tempdir;

    use super::{
        rbuildignore_rule, render, update_agents_file, update_gitignore, update_rbuildignore,
    };
    use crate::config::{Config, EntryKind};
    use crate::digest::TreeDigest;
    use crate::lock::{FetchMethod, LockedPackage, LockedReference, Lockfile};
    use crate::vendor::{VendorResult, VendoredEntry};

    fn fixture() -> (Config, Lockfile, VendorResult) {
        let config = Config::default();
        let lock = Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "2026-06-30T00:00:00Z".into(),
            snapshot: Some("2026-06-30".into()),
            config_digest: format!("sha256:{}", "a".repeat(64)),
            environment_digest: format!("sha256:{}", "b".repeat(64)),
            packages: vec![LockedPackage {
                name: "tinyone".into(),
                version: "1.0.0".into(),
                source: "cran".into(),
                url: Some("https://example.test/tinyone_1.0.0.tar.gz".into()),
                reference: None,
                commit: None,
                fetch_method: FetchMethod::Tarball,
                artifact_digest: format!("sha256:{}", "c".repeat(64)),
                tree_digest: format!("sha256:{}", "d".repeat(64)),
                license: Some("MIT".into()),
            }],
            references: vec![LockedReference {
                name: "standards".into(),
                source: "git::ssh://example.test/standards.git".into(),
                url: None,
                reference: Some("main".into()),
                commit: Some("f".repeat(40)),
                fetch_method: FetchMethod::GitClone,
                artifact_digest: format!("sha256:{}", "1".repeat(64)),
                tree_digest: format!("sha256:{}", "2".repeat(64)),
                license: Some("Apache-2.0".into()),
            }],
        };
        let vendored = VendorResult {
            root: "deps-src".into(),
            warnings: Vec::new(),
            entries: vec![
                VendoredEntry {
                    name: "tinyone".into(),
                    kind: EntryKind::Package,
                    version: Some("1.0.0".into()),
                    license: Some("MIT".into()),
                    title: Some("First Tiny Fixture Package".into()),
                    fetch_method: FetchMethod::Tarball,
                    artifact_sha256: "c".repeat(64),
                    tree: TreeDigest {
                        digest: format!("sha256:{}", "d".repeat(64)),
                        files: BTreeMap::new(),
                    },
                },
                VendoredEntry {
                    name: "standards".into(),
                    kind: EntryKind::Reference,
                    version: None,
                    license: Some("Apache-2.0".into()),
                    title: None,
                    fetch_method: FetchMethod::GitClone,
                    artifact_sha256: "1".repeat(64),
                    tree: TreeDigest {
                        digest: format!("sha256:{}", "2".repeat(64)),
                        files: BTreeMap::new(),
                    },
                },
            ],
        };
        (config, lock, vendored)
    }

    #[test]
    fn markdown_manifest_has_an_insta_snapshot() {
        let (config, lock, vendored) = fixture();
        insta::assert_snapshot!(render(&config, &lock, &vendored).unwrap().markdown);
    }

    #[test]
    fn json_manifest_has_an_insta_snapshot() {
        let (config, lock, vendored) = fixture();
        insta::assert_snapshot!(render(&config, &lock, &vendored).unwrap().json);
    }

    #[test]
    fn reference_description_is_rendered_when_available() {
        let (config, lock, mut vendored) = fixture();
        vendored.entries[1].title = Some("Protocol templates".into());
        let output = render(&config, &lock, &vendored).unwrap();
        assert!(output.json.contains("\"title\": \"Protocol templates\""));
        assert!(output.markdown.contains("| Protocol templates |"));
    }

    #[test]
    fn agents_marker_update_preserves_everything_outside_the_block() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("AGENTS.md");
        let config = Config::parse("[manifest]\nagents-file = true\n").unwrap();
        fs::write(
            &path,
            "User preface\n\n<!-- okr:begin -->\nold generated text\n<!-- okr:end -->\n\nUser suffix\n",
        )
        .unwrap();
        update_agents_file(directory.path(), &config).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with("User preface\n\n<!-- okr:begin -->"));
        assert!(updated.ends_with("<!-- okr:end -->\n\nUser suffix\n"));
        assert!(updated.contains("`deps-src/_manifest.md`"));

        let once = updated.clone();
        update_agents_file(directory.path(), &config).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), once);
    }

    #[test]
    fn agents_file_is_created_and_malformed_markers_are_rejected() {
        let directory = tempdir().unwrap();
        let config = Config::parse("[manifest]\nagents-file = true\n").unwrap();
        update_agents_file(directory.path(), &config).unwrap();
        let path = directory.path().join("AGENTS.md");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("<!-- okr:begin -->")
        );
        fs::write(&path, "<!-- okr:begin -->\nmissing end\n").unwrap();
        assert!(update_agents_file(directory.path(), &config).is_err());
    }

    #[test]
    fn gitignore_entry_is_idempotent_and_preserves_user_lines() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".gitignore");
        fs::write(&path, "/target\n# user comment\n").unwrap();
        update_gitignore(directory.path(), &Config::default()).unwrap();
        update_gitignore(directory.path(), &Config::default()).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "/target\n# user comment\n/deps-src/\n"
        );
    }

    #[test]
    fn rbuildignore_rules_are_idempotent_and_preserve_user_lines() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".Rbuildignore");
        fs::write(&path, "^README[.]md$\n").unwrap();
        let config = Config::parse("[vendor]\npath = \"dep-src\"\n").unwrap();

        update_rbuildignore(directory.path(), &config).unwrap();
        update_rbuildignore(directory.path(), &config).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "^README[.]md$\n^dep-src$\n^okr\\.toml$\n^okr\\.lock$\n"
        );
    }

    #[test]
    fn rbuildignore_is_not_created_and_rules_escape_regex_metacharacters() {
        let directory = tempdir().unwrap();
        update_rbuildignore(directory.path(), &Config::default()).unwrap();
        assert!(!directory.path().join(".Rbuildignore").exists());

        assert_eq!(
            rbuildignore_rule("dep.src+/source(1)/"),
            r"^dep\.src\+/source\(1\)$"
        );
    }

    #[test]
    fn disabled_affordances_do_not_touch_files() {
        let directory = tempdir().unwrap();
        let config =
            Config::parse("[vendor]\ngitignore = false\n[manifest]\nagents-file = false\n")
                .unwrap();
        let agents_path = directory.path().join("AGENTS.md");
        fs::write(&agents_path, "Project-owned instructions\n").unwrap();
        update_agents_file(directory.path(), &config).unwrap();
        update_gitignore(directory.path(), &config).unwrap();
        assert_eq!(
            fs::read_to_string(agents_path).unwrap(),
            "Project-owned instructions\n"
        );
        assert!(!directory.path().join(".gitignore").exists());
    }
}
