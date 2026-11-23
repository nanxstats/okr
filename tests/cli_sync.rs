#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread;

use assert_cmd::cargo::cargo_bin_cmd;
use flate2::Compression;
use flate2::write::GzEncoder;
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
        // Progress is drawn only on an interactive terminal.
        .stderr(predicate::str::is_empty());
    let first_lock = fs::read(project.path().join("okr.lock")).unwrap();
    let first_json = fs::read(project.path().join("deps-src/_manifest.json")).unwrap();
    let first_markdown = fs::read(project.path().join("deps-src/_manifest.md")).unwrap();
    let lock_text = String::from_utf8(first_lock.clone()).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&first_json).unwrap();
    assert!(lock_text.starts_with("version = 2\n"));
    // The repository is the `repo-url` exactly as written. It names no
    // snapshot date, so the lock records none.
    assert!(
        lock_text.contains(&format!(
            "url = \"file://{}/src/contrib/tinyone_1.0.0.tar.gz\"\n",
            fixture_path("fixture-repo/2026-06-30").display()
        )),
        "{lock_text}"
    );
    assert!(!lock_text.contains("snapshot = "), "{lock_text}");
    assert!(
        lock_text.contains("generated = \"1970-01-01T00:00:00Z\"\n"),
        "{lock_text}"
    );
    assert!(!lock_text.contains(".files]"));
    assert!(!lock_text.contains("artifact-digest"));
    assert_eq!(lock_text.matches("tree-digest = \"sha256:").count(), 1);
    assert_eq!(manifest["schema"], 2);
    assert!(manifest["entries"][0].get("files").is_none());
    assert!(manifest["entries"][0].get("artifact_digest").is_none());
    assert!(manifest["entries"][0]["tree_digest"].is_string());
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
fn repo_url_is_used_as_written_whatever_its_path_looks_like() {
    // Regression test for #78: a repository is not required to end in, or
    // contain, a `YYYY-MM-DD` snapshot date.
    for path in [
        "cran/2026-06-30+Y3JhbiwyOjQ1MjYyMTU7",
        "internal/latest",
        "mirror",
    ] {
        let project = tempdir().unwrap();
        let cache = project.path().join("cache");
        let empty_path = project.path().join("empty-path");
        fs::create_dir(&empty_path).unwrap();
        let repository = project.path().join("repositories").join(path);
        copy_tree(&fixture_path("fixture-repo/2026-06-30"), &repository);
        write_repository_config(project.path(), &repository);

        okr(project.path(), &cache, &empty_path)
            .arg("sync")
            .assert()
            .success()
            .stdout(predicate::str::contains("synchronized 1 source entry"));
        let lock_text = fs::read_to_string(project.path().join("okr.lock")).unwrap();
        assert!(
            lock_text.contains(&format!(
                "url = \"file://{}/src/contrib/tinyone_1.0.0.tar.gz\"\n",
                repository.display()
            )),
            "{lock_text}"
        );
        assert!(project.path().join("deps-src/tinyone/R/hello.R").is_file());

        okr(project.path(), &cache, &empty_path)
            .arg("verify")
            .assert()
            .success()
            .stdout(predicate::str::contains("verified sha256:"));
        okr(project.path(), &cache, &empty_path)
            .arg("status")
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "repos=\"file://{}\"",
                repository.display()
            )));
    }
}

#[test]
fn snapshot_selects_the_dated_public_snapshot_and_is_recorded_in_the_lock() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    fs::write(
        project.path().join("okr.toml"),
        "[project]\nsnapshot = \"2026-06-30\"\n\n[packages]\ntinyone = \"*\"\n",
    )
    .unwrap();
    // The snapshot lives on the public Package Manager, so the cache is
    // seeded with what an earlier online sync would have left there.
    let snapshot = "https://packagemanager.posit.co/cran/2026-06-30";
    let seeded = okr::fetch::Cache::new(&cache);
    for file in ["PACKAGES.gz", "tinyone_1.0.0.tar.gz"] {
        seeded
            .put_file(
                &format!("url:{snapshot}/src/contrib/{file}"),
                &fixture_path("fixture-repo/2026-06-30/src/contrib").join(file),
                None,
            )
            .unwrap();
    }

    okr(project.path(), &cache, &empty_path)
        .args(["sync", "--offline"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synchronized 1 source entry"));
    let lock_text = fs::read_to_string(project.path().join("okr.lock")).unwrap();
    assert!(
        lock_text.contains("generated = \"2026-06-30T00:00:00Z\"\nsnapshot = \"2026-06-30\"\n"),
        "{lock_text}"
    );
    assert!(
        lock_text.contains(&format!(
            "url = \"{snapshot}/src/contrib/tinyone_1.0.0.tar.gz\"\n"
        )),
        "{lock_text}"
    );

    okr(project.path(), &cache, &empty_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "install with:  Rscript -e 'pak::pkg_install(c(\"tinyone\"), repos=\"{snapshot}\")'"
        )));
}

#[test]
fn snapshot_and_repo_url_together_are_rejected_with_the_url_to_write_instead() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    fs::write(
        project.path().join("okr.toml"),
        "[project]\nsnapshot = \"2026-06-30\"\nrepo-url = \"https://ppm.example.test/cran\"\n\n[packages]\ntinyone = \"*\"\n",
    )
    .unwrap();

    for command in ["sync", "status", "verify"] {
        okr(project.path(), &cache, &empty_path)
            .arg(command)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(
                "project.snapshot and project.repo-url cannot both be set",
            ))
            .stderr(predicate::str::contains(
                "`repo-url = \"https://ppm.example.test/cran/2026-06-30\"`",
            ));
    }
    assert!(!project.path().join("okr.lock").exists());
}

#[test]
fn a_moving_repo_url_is_followed_and_a_version_that_moved_is_reported() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let repository = project.path().join("repository");
    publish_package(&repository, "tinyone", "1.0.0");
    write_repository_config(project.path(), &repository);

    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let first_lock = fs::read_to_string(project.path().join("okr.lock")).unwrap();
    assert!(first_lock.contains("version = \"1.0.0\"\n"), "{first_lock}");

    // The repository publishes a new version under the same URL. A sync that
    // has to rebuild the tree reads the index again instead of trusting the
    // copy in the cache, and says that the locked version moved.
    publish_package(&repository, "tinyone", "1.1.0");
    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "warning: tinyone was locked at version 1.0.0, but the repository at file://{} now lists 1.1.0; that version will be locked",
            repository.display()
        )));
    let moved_lock = fs::read_to_string(project.path().join("okr.lock")).unwrap();
    assert!(moved_lock.contains("version = \"1.1.0\"\n"), "{moved_lock}");
    assert!(
        moved_lock.contains(&format!(
            "url = \"file://{}/src/contrib/tinyone_1.1.0.tar.gz\"\n",
            repository.display()
        )),
        "{moved_lock}"
    );

    // Offline, the lock and the cache reproduce the tree without the
    // repository.
    fs::remove_dir_all(&repository).unwrap();
    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["sync", "--offline"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert_eq!(
        fs::read_to_string(project.path().join("okr.lock")).unwrap(),
        moved_lock
    );
    okr(project.path(), &cache, &empty_path)
        .arg("verify")
        .assert()
        .success();
}

#[test]
fn verbose_sync_logs_each_source_without_a_terminal() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    write_cran_config(project.path());

    okr(project.path(), &cache, &empty_path)
        .args(["--verbose", "sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synchronized 1 source entry"))
        .stderr(predicate::str::contains("  ✓ tinyone  resolved 1.0.0\n"))
        .stderr(predicate::str::contains("\nResolved 1 source in "))
        .stderr(predicate::str::contains(
            "\n  ✓ tinyone  vendored 1.0.0 (tarball)\n",
        ))
        .stderr(predicate::str::contains("\nVendored 1 source in "))
        // A log has no styling and no live region to redraw.
        .stderr(predicate::str::contains("\u{1b}").not())
        .stderr(predicate::str::contains("Vendoring sources").not());

    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    fs::remove_file(project.path().join("okr.lock")).unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["--verbose", "sync", "--offline"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "\n  ✓ tinyone  vendored 1.0.0 (tarball, cached)\n",
        ));
}

#[test]
fn sync_regenerates_a_lock_written_in_an_older_format() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    write_cran_config(project.path());
    let zeros = "0".repeat(64);
    fs::write(
        project.path().join("okr.lock"),
        format!(
            "version = 1\nokr-version = \"0.1.8\"\ngenerated = \"2026-06-30T00:00:00Z\"\nsnapshot = \"2026-06-30\"\nconfig-digest = \"sha256:{zeros}\"\nenvironment-digest = \"sha256:{zeros}\"\n\n[[package]]\nname = \"tinyone\"\nversion = \"1.0.0\"\nsource = \"cran\"\nurl = \"https://example.test/tinyone_1.0.0.tar.gz\"\nfetch-method = \"tarball\"\nartifact-digest = \"sha256:{zeros}\"\ntree-digest = \"sha256:{zeros}\"\nlicense = \"MIT\"\n"
        ),
    )
    .unwrap();

    okr(project.path(), &cache, &empty_path)
        .arg("verify")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("uses lock version 1"))
        .stderr(predicate::str::contains("run `okr sync`"));

    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "okr.lock uses lock version 1; regenerating it as version 2",
        ))
        .stdout(predicate::str::contains("synchronized 1 source entry"));
    let lock_text = fs::read_to_string(project.path().join("okr.lock")).unwrap();
    assert!(lock_text.starts_with("version = 2\n"));
    assert!(!lock_text.contains("artifact-digest"));

    okr(project.path(), &cache, &empty_path)
        .arg("verify")
        .assert()
        .success()
        .stdout(predicate::str::contains("verified sha256:"));
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
        .stderr(predicate::str::contains(format!(
            "install with:  Rscript -e 'pak::pkg_install(c(\"tinyone\"), repos=\"file://{}\")'",
            fixture_path("fixture-repo/2026-06-30").display()
        )));

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
        .success()
        // Host tools run silently: nothing they print may reach the terminal.
        .stderr(predicate::str::contains("git ").not());
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
fn url_zip_reference_syncs_verifies_and_replays_offline() {
    let project = tempdir().unwrap();
    let cache = project.path().join("cache");
    let empty_path = project.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let archive = fs::read(fixture_path("zip/reference-archive.zip")).unwrap();
    let sha256 = okr::digest::sha256_bytes(&archive);
    let url = serve_archive(archive, "reference-archive.zip");
    let config = |sha256: &str| {
        format!(
            "[vendor]\ngitignore = false\n\n[references]\nnotes = {{ url = \"{url}\", sha256 = \"{sha256}\" }}\n"
        )
    };
    fs::write(project.path().join("okr.toml"), config(&sha256)).unwrap();

    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicate::str::contains("synchronized 1 source entry"));
    let first_lock = fs::read(project.path().join("okr.lock")).unwrap();
    let lock_text = String::from_utf8(first_lock.clone()).unwrap();
    assert!(
        lock_text.contains(&format!("source = \"url::{url}\"\n")),
        "{lock_text}"
    );
    assert!(
        lock_text.contains("fetch-method = \"tarball\"\n"),
        "{lock_text}"
    );
    assert!(lock_text.contains("license = \"MIT\"\n"), "{lock_text}");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(project.path().join("deps-src/_manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["entries"][0]["kind"], "reference");
    assert_eq!(manifest["entries"][0]["source"], format!("url::{url}"));
    assert!(manifest["entries"][0]["commit"].is_null());
    let tree = project.path().join("deps-src/notes");
    assert!(tree.join("README.md").is_file());
    assert!(tree.join("docs/guide.md").is_file());
    assert!(tree.join("tools/check.py").is_file());
    assert!(!tree.join("notes-1.0").exists());

    okr(project.path(), &cache, &empty_path)
        .arg("verify")
        .assert()
        .success()
        .stdout(predicate::str::contains("verified sha256:"));

    fs::remove_dir_all(project.path().join("deps-src")).unwrap();
    okr(project.path(), &cache, &empty_path)
        .args(["sync", "--offline"])
        .assert()
        .success();
    assert_eq!(
        fs::read(project.path().join("okr.lock")).unwrap(),
        first_lock
    );
    assert!(tree.join("docs/guide.md").is_file());

    fs::write(project.path().join("okr.toml"), config(&"0".repeat(64))).unwrap();
    okr(project.path(), &cache, &empty_path)
        .arg("sync")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("SHA-256"));
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
    write_repository_config(project, &fixture_path("fixture-repo/2026-06-30"));
}

/// Declare `tinyone` from the CRAN-like repository rooted at `repository`.
fn write_repository_config(project: &Path, repository: &Path) {
    fs::write(
        project.join("okr.toml"),
        format!(
            "[project]\nrepo-url = \"file://{}\"\n\n[vendor]\ninclude-tests = false\n\n[packages]\ntinyone = \"*\"\n",
            repository.display()
        ),
    )
    .unwrap();
}

/// Publish a minimal source package to the repository rooted at
/// `repository` and list it as the only package in the index.
fn publish_package(repository: &Path, name: &str, version: &str) {
    let contrib = repository.join("src/contrib");
    fs::create_dir_all(&contrib).unwrap();
    let description =
        format!("Package: {name}\nVersion: {version}\nTitle: Fixture\nLicense: MIT\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(description.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    archive
        .append_data(
            &mut header,
            format!("{name}/DESCRIPTION"),
            description.as_bytes(),
        )
        .unwrap();
    let tarball = archive.into_inner().unwrap().finish().unwrap();
    fs::write(contrib.join(format!("{name}_{version}.tar.gz")), tarball).unwrap();

    let mut index = GzEncoder::new(Vec::new(), Compression::default());
    index
        .write_all(format!("Package: {name}\nVersion: {version}\n").as_bytes())
        .unwrap();
    fs::write(contrib.join("PACKAGES.gz"), index.finish().unwrap()).unwrap();
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

/// Serve one archive over plain HTTP for the rest of the test process so a
/// `url::` declaration can be synchronized without live network access.
fn serve_archive(bytes: Vec<u8>, name: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
        }
    });
    format!("http://{address}/{name}")
}

fn host_git() -> bool {
    let Ok(shell) = Shell::new() else {
        return false;
    };
    cmd!(shell, "git --version").quiet().run().is_ok()
}
