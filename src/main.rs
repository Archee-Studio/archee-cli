//! `archee`: the Archee Studio command line: sign in, and serve a folder to the agent.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this crate is the executable boundary"
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "a test asserts by panicking, also in its helpers and async tests"
    )
)]

mod api;
mod cli;
mod connect;
mod journal;
mod models;
mod operations;
#[cfg(test)]
mod operations_tests;
mod output;
mod rfc3339;
mod state;
mod update;
mod users;
mod worker;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;
use crate::output::{CliError, EXIT_USAGE, emit_error};

async fn dispatch(cli: &Cli) -> Result<(), CliError> {
    let Some(command) = &cli.command else {
        return users::home(cli);
    };
    if std::env::var_os(operations::ON_COMPUTER_ENV).is_some() {
        return Err(CliError::new(
            "on_computer",
            "`archee` is not available to commands Archee runs on this computer",
        )
        .exit(EXIT_USAGE));
    }
    users::dispatch(cli, command).await
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emit_error(cli.json, &CliError::io("could not start", &error));
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(&cli)) {
        Ok(()) => {
            // Detached readers must not hold the process open.
            runtime.shutdown_background();
            ExitCode::SUCCESS
        }
        Err(error) => {
            emit_error(cli.json, &error);
            runtime.shutdown_background();
            ExitCode::from(error.exit)
        }
    }
}
