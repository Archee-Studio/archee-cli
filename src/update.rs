//! `archee update`: runs the official installer again, which replaces this binary with the latest
//! release. No background checks, no extra dependency.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::cli::Cli;
use crate::output::{CliError, emit};

#[cfg(unix)]
const INSTALL_COMMAND: &str = "curl -fsSL https://archee.studio/install.sh | sh";
/// Downloaded first: in `curl | sh` a failed download reads as an empty script that succeeds.
#[cfg(unix)]
const INSTALLER: [&str; 3] = [
    "sh",
    "-c",
    "t=$(mktemp) && curl -fsSL https://archee.studio/install.sh -o \"$t\" && sh \"$t\"; s=$?; rm -f \"$t\"; exit $s",
];
#[cfg(windows)]
const INSTALL_COMMAND: &str = "irm https://archee.studio/install.ps1 | iex";
#[cfg(windows)]
const INSTALLER: [&str; 3] = ["powershell", "-NoProfile", "-Command"];
const LIMIT: Duration = Duration::from_secs(300);

fn installer_args() -> Vec<&'static str> {
    let mut args = INSTALLER[1..].to_vec();
    if cfg!(windows) {
        args.push(INSTALL_COMMAND);
    }
    args
}

pub(crate) async fn run(cli: &Cli) -> Result<(), CliError> {
    let mut child = Command::new(INSTALLER[0])
        .args(installer_args())
        .stdout(Stdio::from(std::io::stderr())) // stdout carries only the result, for --json
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| CliError::io("could not start the installer", &error))?;
    match tokio::time::timeout(LIMIT, child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            emit(cli.json, &serde_json::json!({"updated": true}), "updated")
        }
        Ok(Ok(_)) => Err(CliError::new("update_failed", "the installer failed")
            .next(format!("run it yourself: {INSTALL_COMMAND}"))),
        Ok(Err(error)) => Err(CliError::io("the installer did not finish", &error)),
        Err(_) => Err(CliError::new("timeout", "the installer took too long")),
    }
}
