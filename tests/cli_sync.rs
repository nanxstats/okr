#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::tempdir;
use xshell::{Shell, cmd};

#[test]
fn cran_sync_offline_noop_and_mutation_verification_need_no_host_tools() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    write_cran_config(project.path());
    let rbuildignore = project.path().join(".Rbuildignore");
    fs::write(&rbuildignore, "^README[.]md$\n").unwrap();

    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicate::str::contains("synchronized 1 source entry"))
        .stdout(predicate::str::contains("sha256:").not())
        .stdout(predicate::str::contains("Rscript was not found"))
        .stderr(predicate::str::contains("Resolving sources").not())
        .stderr(predicate::str::contains("Preparing tinyone").not());
    let first_lock = fs::read(project.path().join("okr.lock")).unwrap();
    let first_json = fs::read(project.path().join("deps-src/_manifest.json")).unwrap();
    let first_markdown = fs::read(project.path().join("deps-src/_manifest.md")).unwrap();
    let lock_text = String::from_utf8(first_lock.clone()).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&first_json).unwrap();
    assert!(lock_text.starts_with("version = 1\n"));
    assert!(!lock_text.contains(".files]"));
    assert_eq!(manifest["schema"], 1);
    assert!(manifest["entries"][0].get("files").is_none());
    assert!(!project.path().join("AGENTS.md").exists());
    assert_eq!(
        fs::read_to_string(&rbuildignore).unwrap(),
        "^README[.]md$\n^deps-src$\n^okr\\.toml$\n^okr\\.lock$\n"
    );

    fs::write(&rbuildignore, "^README[.]md$\n").unwrap();

    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicate::str::contains("already synchronized; no changes"))
        .stdout(predicate::str::contains("sha256:").not());
    assert!(!project.path().join("AGENTS.md").exists());
    assert_eq!(
        fs::read_to_string(&rbuildignore).unwrap(),
        "^README[.]md$\n^deps-src$\n^okr\\.toml$\n^okr\\.lock$\n"
    );

    okr(project.path(), &cache, &empty_path)
        .args(["--verbose", "sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("environment digest: sha256:"));

    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    fs::remove_file(project.path().join("okr.lock")).unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["--quiet", "sync", "--offline"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
    assert_eq!(
        fs::read(project.path().join("okr.lock")).unwrap(),
        first_lock
    );
    assert_eq!(
        fs::read(project.path().join("deps-src/_manifest.json")).unwrap(),
        first_json
    );
    assert_eq!(
        fs::read(project.path().join("deps-src/_manifest.md")).unwrap(),
        first_markdown
    );

    okr(project.path(), &cache, &empty_path)
        .arg("verify")
        .assert()
        .success()
        .stdout(predicate::str::contains("verified sha256:"));

    fs::write(
        project.path().join("deps-src/tinyone/R/hello.R"),
        b"hello <- function() 'tampered'\n",
    )
    .unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["verify", "--json"])
        .assert()
        .code(4)
        .stdout(predicate::str::contains("\"schema\": 1"))
        .stdout(predicate::str::contains("\"path\": \".\""))
        .stdout(predicate::str::contains("\"mismatch\": \"modified\""));
}

#[test]
fn coherence_warns_by_default_and_is_fatal_only_when_strict() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let tools = project.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let rscript = tools.join("Rscript");
    fs::write(
        &rscript,
        "#!/bin/sh\nprintf '__OKR_RLIB_INSPECTION_V1_BEGIN__\\n4.5.1\\ntinyone\\t9.9.9\\n__OKR_RLIB_INSPECTION_V1_END__\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&rscript, fs::Permissions::from_mode(0o755)).unwrap();
    write_cran_config(project.path());

    okr(project.path(), &cache, &tools)
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::contains("installed 9.9.9, vendored 1.0.0"))
        .stderr(predicate::str::contains("install with:  Rscript -e"));

    okr(project.path(), &cache, &tools)
        .args(["sync", "--strict"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("coherence failed"));

    okr(project.path(), &cache, &tools)
        .args(["verify", "--strict", "--json"])
        .assert()
        .code(4)
        .stdout(predicate::str::contains("\"status\": \"mismatch\""))
        .stdout(predicate::str::contains("\"installed_version\": \"9.9.9\""));
}

#[test]
fn file_git_reference_with_a_source_link_rebuilds_offline_from_the_normalized_clone_cache() {
    if !host_git() {
        return;
    }
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let repository = project.path().join("reference-repository");
    copy_tree(&fixture_path("reference-repo"), &repository);
    fs::create_dir_all(repository.join("xDir/pkg")).unwrap();
    fs::write(repository.join("xDir/pkg/context.txt"), "linked context\n").unwrap();
    symlink("xDir/pkg", repository.join("linked-context")).unwrap();
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
    let url = format!("file://{}", repository.display());
    fs::write(
        project.path().join("okr.toml"),
        format!("[vendor]\ngitignore = false\n[references]\nstandards = \"git::{url}@{commit}\"\n"),
    )
    .unwrap();

    let normal_path = std::env::var_os("PATH").unwrap();
    okr(project.path(), &cache, Path::new(&normal_path))
        .arg("sync")
        .assert()
        .success();
    let first_lock = fs::read(project.path().join("okr.lock")).unwrap();
    let first_manifest = fs::read(project.path().join("deps-src/_manifest.json")).unwrap();
    let first_markdown = fs::read(project.path().join("deps-src/_manifest.md")).unwrap();
    let source_link = project.path().join("deps-src/standards/linked-context");
    assert!(
        fs::symlink_metadata(&source_link)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(fs::read(&source_link).unwrap(), b"xDir/pkg");
    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    fs::remove_dir_all(&repository).unwrap();
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();

    okr(project.path(), &cache, &empty_path)
        .args(["sync", "--offline"])
        .assert()
        .success();
    assert_eq!(
        fs::read(project.path().join("okr.lock")).unwrap(),
        first_lock
    );
    assert_eq!(
        fs::read(project.path().join("deps-src/_manifest.json")).unwrap(),
        first_manifest
    );
    assert_eq!(
        fs::read(project.path().join("deps-src/_manifest.md")).unwrap(),
        first_markdown
    );
    assert!(
        project
            .path()
            .join("deps-src/standards/docs/guide.md")
            .is_file()
    );
    let source_link = project.path().join("deps-src/standards/linked-context");
    assert!(
        fs::symlink_metadata(&source_link)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(fs::read(source_link).unwrap(), b"xDir/pkg");
}

#[test]
fn sync_explains_when_a_reference_was_declared_as_a_package() {
    if !host_git() {
        return;
    }
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let repository = project.path().join("not-an-r-package");
    fs::create_dir(&repository).unwrap();
    fs::write(repository.join("README.md"), "# Reference project\n").unwrap();
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
    cmd!(shell, "git -C {repository} add README.md")
        .run()
        .unwrap();
    cmd!(shell, "git -C {repository} commit -q -m fixture")
        .run()
        .unwrap();
    let commit = cmd!(shell, "git -C {repository} rev-parse HEAD")
        .read()
        .unwrap();
    let source = format!("git::file://{}@{commit}", repository.display());
    fs::write(
        project.path().join("okr.toml"),
        format!("[packages]\nnot-an-r-package = \"{source}\"\n"),
    )
    .unwrap();
    let normal_path = std::env::var_os("PATH").unwrap();

    okr(project.path(), &cache, Path::new(&normal_path))
        .arg("sync")
        .assert()
        .code(3)
        .stderr(predicate::str::contains(
            "source `not-an-r-package` was declared in `[packages]`",
        ))
        .stderr(predicate::str::contains(
            "move its `not-an-r-package` declaration from `[packages]` to `[references]`",
        ))
        .stderr(predicate::str::contains(format!(
            "okr add {source} --reference"
        )));
}

fn okr(project: &Path, cache: &Path, path: &Path) -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("okr");
    command
        .current_dir(project)
        .env("OKR_CACHE_DIR", cache)
        .env("PATH", path);
    command
}

fn write_cran_config(project: &Path) {
    let repository = fixture_path("fixture-repo");
    fs::write(
        project.join("okr.toml"),
        format!(
            "[project]\nsnapshot = \"2026-06-30\"\nrepo-url = \"file://{}\"\n\n[vendor]\ninclude-tests = false\n\n[packages]\ntinyone = \"*\"\n",
            repository.display()
        ),
    )
    .unwrap();
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}

fn copy_tree(source: &Path, destination: &Path) {
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

fn host_git() -> bool {
    let Ok(shell) = Shell::new() else {
        return false;
    };
    cmd!(shell, "git --version").quiet().run().is_ok()
}
