use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use okr::cli::Cli;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let code = error
                .downcast_ref::<okr::Error>()
                .map_or(1, okr::Error::exit_code);
            eprintln!("error: {error:#}");
            ExitCode::from(code)
        }
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    okr::cli::run(cli).context("okr command failed")
}
