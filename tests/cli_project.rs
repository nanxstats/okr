#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn init_writes_the_safe_template_and_requires_force_to_replace_it() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();

    okr(project.path(), &cache, &empty_path)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote okr.toml"));
    let template = fs::read_to_string(project.path().join("okr.toml")).unwrap();
    assert!(template.contains("[packages]"));
    assert!(template.contains("[references]"));
    assert!(template.contains("# snapshot"));
    assert_eq!(
        fs::read_to_string(project.path().join(".gitignore")).unwrap(),
        "/deps-src/\n"
    );
    okr(project.path(), &cache, &empty_path)
        .arg("init")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--force"));

    fs::write(project.path().join("okr.toml"), "user contents\n").unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["init", "--force"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(project.path().join("okr.toml")).unwrap(),
        template
    );
    assert_eq!(
        fs::read_to_string(project.path().join(".gitignore")).unwrap(),
        "/deps-src/\n"
    );

    okr(project.path(), &cache, &empty_path)
        .args(["init", "--profile", "clinical-trials", "--force"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("milestone 0.2"));
}

#[test]
fn add_is_transactional_and_preserves_comments_and_formatting() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let config = r#"# precious heading
[project] # keep project comment
snapshot = "2026-06-30"
strict=false # keep compact formatting

[packages] # package comment
# keep this package note

[references]
"#;
    fs::write(project.path().join("okr.toml"), config).unwrap();

    okr(project.path(), &cache, &empty_path)
        .args(["add", "pharmaverse/admiral@v1.3.0", "tinyone"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added admiral"))
        .stdout(predicate::str::contains("added tinyone"));
    okr(project.path(), &cache, &empty_path)
        .args(["add", "--reference", "git::file:///tmp/standards.git@main"])
        .assert()
        .success();
    let updated = fs::read_to_string(project.path().join("okr.toml")).unwrap();
    assert!(updated.starts_with("# precious heading\n"));
    assert!(updated.contains("[project] # keep project comment"));
    assert!(updated.contains("strict=false # keep compact formatting"));
    assert!(updated.contains("[packages] # package comment"));
    assert!(updated.contains("# keep this package note"));
    let parsed = okr::config::Config::parse(&updated).unwrap();
    assert!(parsed.packages.contains_key("admiral"));
    assert!(parsed.packages.contains_key("tinyone"));
    assert!(parsed.references.contains_key("standards"));

    let before_error = updated.into_bytes();
    okr(project.path(), &cache, &empty_path)
        .args(["add", "pharmaverse/admiral@v1.3.0"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
    assert_eq!(
        fs::read(project.path().join("okr.toml")).unwrap(),
        before_error
    );

    okr(project.path(), &cache, &empty_path)
        .args(["add", "bioc::BiocGenerics"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("milestone 0.2"));
    okr(project.path(), &cache, &empty_path)
        .args(["add", "url::https://example.test/pkg.tar.gz"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("sha256"));
}

#[test]
fn status_reports_tools_freshness_integrity_cache_and_machine_json() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let repository = fixture_path("fixture-repo");
    fs::write(
        project.path().join("okr.toml"),
        format!(
            "[project]\nsnapshot = \"2026-06-30\"\nrepo-url = \"file://{}\"\n[packages]\ntinyone = \"*\"\n",
            repository.display()
        ),
    )
    .unwrap();
    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success();

    okr(project.path(), &cache, &empty_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("R: absent"))
        .stdout(predicate::str::contains(
            "tools: git unavailable, gh unavailable",
        ))
        .stdout(predicate::str::contains("lock: fresh"))
        .stdout(predicate::str::contains("vendor: clean"))
        .stdout(predicate::str::contains("cache:"))
        .stdout(predicate::str::contains("install with:  Rscript -e"));

    let output = okr(project.path(), &cache, &empty_path)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["r"]["status"], "absent");
    assert_eq!(json["tools"]["git"], false);
    assert_eq!(json["tools"]["gh"], false);
    assert_eq!(json["lock"]["fresh"], true);
    assert_eq!(json["vendor"]["status"], "clean");

    fs::write(
        project.path().join("deps-src/tinyone/R/hello.R"),
        b"tampered\n",
    )
    .unwrap();
    okr(project.path(), &cache, &empty_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("vendor: drift"));
}

fn okr(project: &Path, cache: &Path, path: &Path) -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("okr");
    command
        .current_dir(project)
        .env("OKR_CACHE_DIR", cache)
        .env("PATH", path);
    command
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}
