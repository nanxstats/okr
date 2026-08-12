//! Command-line parsing and command dispatch.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value, value};

use crate::config::Config;
use crate::fetch::{Cache, Fetcher};
use crate::hosttools::HostTools;
use crate::lock::{Lockfile, VerificationReport, config_hash, verify_vendor};
use crate::manifest::{update_agents_file, update_gitignore, write_manifests};
use crate::resolve::{TieredGithubApi, resolve};
use crate::rlib::{CoherenceReport, CoherenceStatus, Inspection, check_coherence};
use crate::spec::RemoteSpec;
use crate::vendor::vendor;
use crate::{Error, Result};

#[derive(Debug, Parser)]
#[command(
    name = "okr",
    version,
    about,
    after_long_help = "Examples:\n  okr init\n  okr add pharmaverse/admiral@v1.3.0\n  okr sync\n  okr verify --strict --json\n\nokr retrieves and verifies source context. It never installs R or R packages."
)]
pub struct Cli {
    /// Path to the project configuration.
    #[arg(long, global = true, default_value = "okr.toml")]
    pub config: PathBuf,

    /// Suppress non-error human output.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Print additional diagnostic detail.
    #[arg(long, global = true, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Emit schema-versioned JSON for status or verify.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a project configuration.
    Init(InitArgs),
    /// Add package or reference source declarations.
    Add(AddArgs),
    /// Resolve, fetch, vendor, lock, and diagnose the project.
    Sync(SyncArgs),
    /// Report project, tool, cache, and coherence status.
    Status(StatusArgs),
    /// Verify the full vendored tree against the lockfile.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Profiles are planned for milestone 0.2. Omit --profile to write the milestone 0.1 template."
)]
pub struct InitArgs {
    /// Profile name or path (profiles arrive in milestone 0.2).
    #[arg(long)]
    pub profile: Option<String>,

    /// Replace an existing configuration.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Examples:\n  okr add rpact\n  okr add pharmaverse/admiral@v1.3.0\n  okr add github::tidyverse/ggplot2\n  okr add r-lib/testthat@*release\n  okr add gitlab::jimhester/covr@abc123\n  okr add bitbucket::sulab/mygene.r@default\n  okr add git::git@ghe.example:stats/simlib.git@v2.1\n  okr add --reference git::https://codeberg.org/org/protocols.git@main\n\nDirect url:: tarballs require table form in okr.toml so a sha256 can be declared. Bare names such as `rpact` add a CRAN `*` entry and require project.snapshot."
)]
pub struct AddArgs {
    /// Source specifications to add.
    #[arg(required = true)]
    pub specs: Vec<String>,

    /// Add entries under [references] instead of [packages].
    #[arg(long)]
    pub reference: bool,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Examples:\n  okr sync\n  okr sync --offline\n  okr sync --strict\n\n--offline performs no downloads or clones. It requires the resolved artifacts in the content-addressed cache."
)]
pub struct SyncArgs {
    /// Prohibit network and clone operations; require cache hits.
    #[arg(long)]
    pub offline: bool,

    /// Treat installed-library coherence mismatches as failures.
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Examples:\n  okr status\n  okr status --json\n\nStatus is diagnostic: it never installs or changes R packages. project.strict makes an installed-library mismatch exit 4."
)]
pub struct StatusArgs {}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Examples:\n  okr verify\n  okr verify --json\n  okr verify --strict --json\n\nTree drift is always fatal (exit 4). --strict additionally makes installed-library coherence drift fatal."
)]
pub struct VerifyArgs {
    /// Also require installed-library coherence.
    #[arg(long)]
    pub strict: bool,
}

pub fn run(cli: Cli) -> Result<()> {
    let Cli {
        config,
        quiet,
        verbose,
        json,
        command,
    } = cli;
    match command {
        Command::Sync(arguments) => {
            reject_json(json, "sync")?;
            run_sync(&config, &arguments, quiet, verbose)
        }
        Command::Verify(arguments) => run_verify(&config, &arguments, json, quiet),
        Command::Init(arguments) => {
            reject_json(json, "init")?;
            run_init(&config, &arguments, quiet)
        }
        Command::Add(arguments) => {
            reject_json(json, "add")?;
            run_add(&config, &arguments, quiet)
        }
        Command::Status(_) => run_status(&config, json, quiet, verbose),
    }
}

const DEFAULT_CONFIG: &str = r#"[project]
# name = "my-r-project"
# r-version = "4.5.1"      # advisory only
# snapshot = "2026-06-30"  # required when adding CRAN packages
strict = false

[vendor]
path = "deps-src"
include-tests = true
exclude = []
gitignore = true

[manifest]
agents-file = true

[packages]

[references]
"#;

fn run_init(config_path: &Path, args: &InitArgs, quiet: bool) -> Result<()> {
    if args.profile.is_some() {
        return Err(Error::Config(
            "profiles are planned for milestone 0.2; omit --profile for the milestone 0.1 template"
                .into(),
        ));
    }
    if config_path.try_exists()? && !args.force {
        return Err(Error::Config(format!(
            "{} already exists; pass --force to replace it",
            config_path.display()
        )));
    }
    let config = Config::parse(DEFAULT_CONFIG)?;
    atomic_write_preserving_permissions(config_path, DEFAULT_CONFIG)?;
    let project = project_directory(config_path);
    update_gitignore(&project, &config)?;
    if !quiet {
        println!("wrote {}", config_path.display());
        println!(
            "managed /{}/ in {}",
            config.vendor.path.display(),
            project.join(".gitignore").display()
        );
    }
    Ok(())
}

fn run_add(config_path: &Path, args: &AddArgs, quiet: bool) -> Result<()> {
    let input = fs::read_to_string(config_path).map_err(|error| {
        Error::Config(format!(
            "could not read configuration {}: {error}",
            config_path.display()
        ))
    })?;
    Config::parse(&input)?;
    let mut document = input.parse::<DocumentMut>().map_err(|error| {
        Error::Config(format!(
            "invalid {} for editing: {error}",
            config_path.display()
        ))
    })?;
    let section = if args.reference {
        "references"
    } else {
        "packages"
    };
    if !document.as_table().contains_key(section) {
        document[section] = Item::Table(Table::new());
    }

    let other_section = if args.reference {
        "packages"
    } else {
        "references"
    };
    let mut added = Vec::new();
    for raw in &args.specs {
        let (name, stored) = if raw.contains('/') || raw.contains("::") {
            let parsed = RemoteSpec::parse(raw)?;
            (parsed.suggested_name(), raw.clone())
        } else if args.reference {
            return Err(Error::Spec(format!(
                "reference `{raw}` is CRAN-shaped; [references] requires a git or url source"
            )));
        } else {
            if raw.is_empty() || raw.contains(['@', '#']) {
                return Err(Error::Spec(format!(
                    "invalid CRAN package name `{raw}`; use a bare package name"
                )));
            }
            (raw.clone(), "*".into())
        };
        if editable_section_contains(&document, section, &name)? {
            return Err(Error::Config(format!(
                "[{section}].{name} already exists; edit it directly to change its source"
            )));
        }
        if editable_section_contains(&document, other_section, &name)? {
            return Err(Error::Config(format!(
                "entry name `{name}` already exists in [{other_section}]"
            )));
        }
        editable_section_insert(&mut document, section, &name, stored)?;
        added.push(name);
    }

    let output = document.to_string();
    Config::parse(&output)?;
    atomic_write_preserving_permissions(config_path, &output)?;
    if !quiet {
        for name in added {
            println!("added {name} to [{section}]");
        }
    }
    Ok(())
}

fn editable_section_contains(document: &DocumentMut, section: &str, name: &str) -> Result<bool> {
    let Some(item) = document.get(section) else {
        return Ok(false);
    };
    if let Some(table) = item.as_table() {
        Ok(table.contains_key(name))
    } else if let Some(table) = item.as_inline_table() {
        Ok(table.contains_key(name))
    } else {
        Err(Error::Config(format!(
            "`{section}` must be a TOML table or inline table"
        )))
    }
}

fn editable_section_insert(
    document: &mut DocumentMut,
    section: &str,
    name: &str,
    stored: String,
) -> Result<()> {
    let item = document
        .get_mut(section)
        .ok_or_else(|| Error::Config(format!("missing [{section}] section while applying edit")))?;
    if let Some(table) = item.as_table_mut() {
        table.insert(name, value(stored));
        Ok(())
    } else if let Some(table) = item.as_inline_table_mut() {
        table.insert(name, Value::from(stored));
        Ok(())
    } else {
        Err(Error::Config(format!(
            "`{section}` must be a TOML table or inline table"
        )))
    }
}

fn run_status(config_path: &Path, json: bool, quiet: bool, _verbose: bool) -> Result<()> {
    let project = project_directory(config_path);
    let config = Config::load(config_path)?;
    let lock_path = project.join("okr.lock");
    let lock = Lockfile::load_optional(&lock_path)?;
    let tools = HostTools::new().availability();
    let cache = Cache::from_environment()?;
    let cache_stats = cache.stats()?;
    let inspection = crate::rlib::inspect();
    let current_config_hash = config_hash(&config)?;
    let lock_fresh = lock
        .as_ref()
        .map(|lock| lock.config_hash == current_config_hash);
    let verification = lock
        .as_ref()
        .map(|lock| verify_vendor(&project, &config, lock));
    let coherence = lock.as_ref().map(|lock| check_coherence(lock, &inspection));
    let install = lock
        .as_ref()
        .and_then(|lock| (!lock.packages.is_empty()).then(|| install_command(&config, lock)));
    let r_status = status_r(&inspection, config.project.r_version.as_deref());

    if json {
        let output = StatusJson {
            schema: 1,
            r: r_status,
            tools,
            lock: StatusLock {
                present: lock.is_some(),
                fresh: lock_fresh,
                environment_digest: lock.as_ref().map(|lock| lock.environment_digest.clone()),
            },
            vendor: StatusVendor {
                status: verification.as_ref().map_or("unlocked", |report| {
                    if report.is_clean() { "clean" } else { "drift" }
                }),
                mismatch_count: verification
                    .as_ref()
                    .map_or(0, |report| report.mismatches.len()),
            },
            coherence: coherence.as_ref(),
            cache: StatusCache {
                path: cache.root().display().to_string(),
                artifacts: cache_stats.artifacts,
                bytes: cache_stats.bytes,
            },
            install: install.as_deref(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|error| Error::Io(std::io::Error::other(error)))?
        );
    } else if !quiet {
        print_human_status(
            &r_status,
            tools,
            lock.as_ref(),
            lock_fresh,
            verification.as_ref(),
            coherence.as_ref(),
            cache.root(),
            cache_stats.artifacts,
            cache_stats.bytes,
            install.as_deref(),
        );
    }

    if config.project.strict
        && coherence
            .as_ref()
            .is_some_and(CoherenceReport::has_mismatches)
    {
        return Err(Error::Verification(
            "project.strict is true and installed-library coherence failed".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct StatusR {
    status: &'static str,
    version: Option<String>,
    note: Option<String>,
    advisory_match: Option<bool>,
}

#[derive(Serialize)]
struct StatusJson<'a> {
    schema: u32,
    r: StatusR,
    tools: crate::hosttools::Availability,
    lock: StatusLock,
    vendor: StatusVendor,
    coherence: Option<&'a CoherenceReport>,
    cache: StatusCache,
    install: Option<&'a str>,
}

#[derive(Serialize)]
struct StatusLock {
    present: bool,
    fresh: Option<bool>,
    environment_digest: Option<String>,
}

#[derive(Serialize)]
struct StatusVendor {
    status: &'static str,
    mismatch_count: usize,
}

#[derive(Serialize)]
struct StatusCache {
    path: String,
    artifacts: u64,
    bytes: u64,
}

fn status_r(inspection: &Inspection, expected: Option<&str>) -> StatusR {
    match inspection {
        Inspection::Absent => StatusR {
            status: "absent",
            version: None,
            note: Some("Rscript not found; advisory checks skipped".into()),
            advisory_match: None,
        },
        Inspection::Unavailable { reason } => StatusR {
            status: "unavailable",
            version: None,
            note: Some(reason.clone()),
            advisory_match: None,
        },
        Inspection::Available { r_version, .. } => StatusR {
            status: "available",
            version: Some(r_version.clone()),
            note: None,
            advisory_match: expected.map(|expected| expected == r_version),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn print_human_status(
    r: &StatusR,
    tools: crate::hosttools::Availability,
    lock: Option<&Lockfile>,
    fresh: Option<bool>,
    verification: Option<&VerificationReport>,
    coherence: Option<&CoherenceReport>,
    cache: &Path,
    artifacts: u64,
    bytes: u64,
    install: Option<&str>,
) {
    match &r.version {
        Some(version) => println!("R: {version} ({})", r.status),
        None => println!(
            "R: {} ({})",
            r.status,
            r.note.as_deref().unwrap_or("no detail")
        ),
    }
    if r.advisory_match == Some(false) {
        println!("R version advisory: mismatch");
    }
    println!(
        "tools: git {}, gh {}",
        availability_word(tools.git),
        availability_word(tools.gh)
    );
    match (lock, fresh) {
        (None, _) => println!("lock: missing"),
        (Some(lock), Some(true)) => {
            println!("lock: fresh ({})", lock.environment_digest);
        }
        (Some(lock), Some(false)) => {
            println!("lock: stale config hash ({})", lock.environment_digest);
        }
        (Some(_), None) => println!("lock: present"),
    }
    match verification {
        None => println!("vendor: unlocked"),
        Some(report) if report.is_clean() => println!("vendor: clean"),
        Some(report) => println!("vendor: drift ({} mismatch(es))", report.mismatches.len()),
    }
    if let Some(coherence) = coherence {
        println!("coherence: {}", coherence_status_word(coherence.status));
        for mismatch in &coherence.mismatches {
            println!(
                "  {}: installed {}, vendored {}",
                mismatch.package,
                mismatch
                    .installed_version
                    .as_deref()
                    .unwrap_or("<not installed>"),
                mismatch.vendored_version
            );
        }
    } else {
        println!("coherence: not checked (no lock)");
    }
    println!(
        "cache: {artifacts} artifact(s), {bytes} byte(s) at {}",
        cache.display()
    );
    if let Some(install) = install {
        println!("install with:  {install}");
    }
}

const fn availability_word(available: bool) -> &'static str {
    if available {
        "available"
    } else {
        "unavailable"
    }
}

const fn coherence_status_word(status: CoherenceStatus) -> &'static str {
    match status {
        CoherenceStatus::Clean => "clean",
        CoherenceStatus::Mismatch => "mismatch",
        CoherenceStatus::Skipped => "skipped",
        CoherenceStatus::Unavailable => "unavailable",
    }
}

fn run_sync(
    config_path: &std::path::Path,
    args: &SyncArgs,
    quiet: bool,
    verbose: bool,
) -> Result<()> {
    let project_directory = project_directory(config_path);
    let config = Config::load(config_path)?;
    let lock_path = project_directory.join("okr.lock");
    let previous = Lockfile::load_optional(&lock_path)?;
    let expected_config_hash = config_hash(&config)?;
    let fresh_previous = previous
        .as_ref()
        .filter(|lock| lock.config_hash == expected_config_hash);

    if let Some(lock) = fresh_previous
        && lock.okr_version == env!("CARGO_PKG_VERSION")
        && verify_vendor(&project_directory, &config, lock).is_clean()
        && manifests_exist(&project_directory, &config)
    {
        update_agents_file(&project_directory, &config)?;
        update_gitignore(&project_directory, &config)?;
        let coherence = check_coherence(lock, &crate::rlib::inspect());
        emit_coherence(&config, lock, &coherence, quiet);
        if (args.strict || config.project.strict) && coherence.has_mismatches() {
            return Err(Error::Verification(format!(
                "installed-library coherence failed for {} package(s)",
                coherence.mismatches.len()
            )));
        }
        if !quiet {
            println!(
                "already synchronized; no changes ({})",
                lock.environment_digest
            );
        }
        return Ok(());
    }

    let cache = Cache::from_environment()?;
    let fetcher = Fetcher::new(cache, args.offline)?;
    let tools = HostTools::new();
    let github = TieredGithubApi::new(&tools)?;
    let resolution = resolve(&config, &fetcher, &tools, &github, fresh_previous)?;
    let vendored = vendor(&project_directory, &config, &resolution, &fetcher, &tools)?;
    let lock = Lockfile::build(&config, &resolution, &vendored)?;
    lock.write(&lock_path)?;
    write_manifests(&config, &lock, &vendored)?;
    update_agents_file(&project_directory, &config)?;
    update_gitignore(&project_directory, &config)?;

    if !quiet {
        for warning in &vendored.warnings {
            eprintln!("warning: {warning}");
        }
        println!(
            "synchronized {} source entr{} ({})",
            vendored.entries.len(),
            if vendored.entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            lock.environment_digest
        );
        if verbose {
            let stats = fetcher.cache().stats()?;
            println!(
                "cache: {} artifact(s), {} byte(s) at {}",
                stats.artifacts,
                stats.bytes,
                fetcher.cache().root().display()
            );
        }
    }

    let coherence = check_coherence(&lock, &crate::rlib::inspect());
    emit_coherence(
        &config,
        &lock,
        &coherence,
        quiet || !verbose && coherence.status == CoherenceStatus::Clean,
    );
    let strict = args.strict || config.project.strict;
    if strict && coherence.has_mismatches() {
        return Err(Error::Verification(format!(
            "installed-library coherence failed for {} package(s)",
            coherence.mismatches.len()
        )));
    }
    Ok(())
}

fn run_verify(
    config_path: &std::path::Path,
    args: &VerifyArgs,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let project_directory = project_directory(config_path);
    let config = Config::load(config_path)?;
    let lock = Lockfile::load(&project_directory.join("okr.lock"))?;
    let tree = verify_vendor(&project_directory, &config, &lock);
    let strict = args.strict || config.project.strict;
    let coherence = strict.then(|| check_coherence(&lock, &crate::rlib::inspect()));
    let coherence_failed = coherence
        .as_ref()
        .is_some_and(CoherenceReport::has_mismatches);
    let ok = tree.is_clean() && !coherence_failed;

    if json {
        let output = VerifyJson {
            schema: 1,
            ok,
            environment_digest: &tree.environment_digest,
            mismatches: &tree.mismatches,
            coherence: coherence.as_ref(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|error| Error::Io(std::io::Error::other(error)))?
        );
    } else if !quiet {
        emit_tree_report(&tree);
        if let Some(coherence) = &coherence {
            emit_coherence(&config, &lock, coherence, false);
        }
    }

    if ok {
        Ok(())
    } else {
        Err(Error::Verification(format!(
            "verification failed with {} tree mismatch(es){}",
            tree.mismatches.len(),
            if coherence_failed {
                " and an installed-library coherence mismatch"
            } else {
                ""
            }
        )))
    }
}

#[derive(Serialize)]
struct VerifyJson<'a> {
    schema: u32,
    ok: bool,
    environment_digest: &'a str,
    mismatches: &'a [crate::lock::FileMismatch],
    coherence: Option<&'a CoherenceReport>,
}

fn emit_tree_report(report: &VerificationReport) {
    if report.is_clean() {
        println!("verified {}", report.environment_digest);
        return;
    }
    eprintln!(
        "verification failed: {} mismatch(es)",
        report.mismatches.len()
    );
    for mismatch in &report.mismatches {
        eprintln!(
            "  {}/{}: {} ({:?})",
            mismatch.entry, mismatch.path, mismatch.entry_kind, mismatch.mismatch
        );
    }
}

fn emit_coherence(config: &Config, lock: &Lockfile, report: &CoherenceReport, quiet: bool) {
    match report.status {
        CoherenceStatus::Skipped => {
            if !quiet {
                println!(
                    "note: {}",
                    report.note.as_deref().unwrap_or("coherence check skipped")
                );
            }
        }
        CoherenceStatus::Unavailable => {
            if !quiet {
                eprintln!(
                    "warning: {}",
                    report
                        .note
                        .as_deref()
                        .unwrap_or("installed-library coherence could not be checked")
                );
            }
        }
        CoherenceStatus::Clean => {
            if !quiet {
                println!("installed-library coherence: clean");
            }
        }
        CoherenceStatus::Mismatch => {
            eprintln!("warning: installed R library does not match vendored sources:");
            for mismatch in &report.mismatches {
                eprintln!(
                    "  {}: installed {}, vendored {}",
                    mismatch.package,
                    mismatch
                        .installed_version
                        .as_deref()
                        .unwrap_or("<not installed>"),
                    mismatch.vendored_version
                );
            }
            eprintln!("install with:  {}", install_command(config, lock));
        }
    }
    if let (Some(expected), Some(actual)) = (&config.project.r_version, &report.r_version)
        && expected != actual
        && !quiet
    {
        eprintln!(
            "warning: project.r-version is {expected}, but detected R {actual} (advisory only)"
        );
    }
}

#[must_use]
pub fn install_command(config: &Config, lock: &Lockfile) -> String {
    let targets = lock
        .packages
        .iter()
        .map(|package| {
            if package.source == "cran" {
                package.name.clone()
            } else if let Some(commit) = &package.commit {
                format!("{}@{commit}", package.source)
            } else {
                package.source.clone()
            }
        })
        .map(|target| format!("\"{}\"", escape_r_string(&target)))
        .collect::<Vec<_>>()
        .join(",");
    let repository = lock.snapshot.as_ref().map(|snapshot| {
        format!(
            ", repos=\"{}/{snapshot}\"",
            escape_r_string(config.repository_url())
        )
    });
    let expression = format!(
        "pak::pkg_install(c({targets}){})",
        repository.as_deref().unwrap_or("")
    );
    format!("Rscript -e '{}'", expression.replace('\'', "'\"'\"'"))
}

fn escape_r_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn manifests_exist(project_directory: &std::path::Path, config: &Config) -> bool {
    let root = project_directory.join(&config.vendor.path);
    root.join("_manifest.json").is_file() && root.join("_manifest.md").is_file()
}

fn project_directory(config_path: &std::path::Path) -> PathBuf {
    let parent = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    if parent.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        parent.to_owned()
    }
}

fn reject_json(enabled: bool, command: &str) -> Result<()> {
    if enabled {
        Err(Error::Config(format!(
            "--json is not supported by `{command}`; use it with `status` or `verify`"
        )))
    } else {
        Ok(())
    }
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
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
