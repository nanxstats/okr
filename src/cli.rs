//! Command-line parsing and command dispatch.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
    let command = match cli.command {
        Command::Init(_) => "init",
        Command::Add(_) => "add",
        Command::Sync(_) => "sync",
        Command::Status => "status",
        Command::Verify(_) => "verify",
    };

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
