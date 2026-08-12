//! Command-line parsing and command dispatch.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::config::Config;
use crate::fetch::{Cache, Fetcher};
use crate::hosttools::HostTools;
use crate::lock::{Lockfile, VerificationReport, config_hash, verify_vendor};
use crate::manifest::{update_agents_file, update_gitignore, write_manifests};
use crate::resolve::{TieredGithubApi, resolve};
use crate::rlib::{CoherenceReport, CoherenceStatus, check_coherence};
use crate::vendor::vendor;
use crate::{Error, Result};

#[derive(Debug, Parser)]
#[command(name = "okr", version, about)]
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

    /// Emit schema-versioned JSON where the command supports it.
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
    Status,
    /// Verify the full vendored tree against the lockfile.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Profile name or path (profiles arrive in milestone 0.2).
    #[arg(long)]
    pub profile: Option<String>,

    /// Replace an existing configuration.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Source specifications to add.
    #[arg(required = true)]
    pub specs: Vec<String>,

    /// Add entries under [references] instead of [packages].
    #[arg(long)]
    pub reference: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Prohibit network and clone operations; require cache hits.
    #[arg(long)]
    pub offline: bool,

    /// Treat installed-library coherence mismatches as failures.
    #[arg(long)]
    pub strict: bool,
}

#[derive(Debug, Args)]
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
        Command::Init(_) => not_implemented("init"),
        Command::Add(_) => not_implemented("add"),
        Command::Status => not_implemented("status"),
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

fn not_implemented<T>(command: &str) -> Result<T> {
    Err(Error::NotImplemented(format!(
        "the {command} command is not implemented"
    )))
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
