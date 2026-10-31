//! The command surface. Parsing only: see `users`, `connect` and `worker` for what runs.

use clap::{Parser, Subcommand};

/// The product name users see in help and errors.
pub(crate) const PRODUCT_NAME: &str = "Archee Studio";

#[derive(Debug, Parser)]
#[command(
    name = "archee",
    version,
    disable_help_subcommand = true,
    about = format!("{PRODUCT_NAME} from your terminal: sign in, and let Archee work in a folder on your computer.")
)]
pub(crate) struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

// The order here is the order `--help` shows.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Sign in with a short code in your browser, or store ARCHEE_TOKEN.
    Login,
    /// Let Archee work in this folder on your computer, until you press Ctrl-C.
    Connect {
        /// The folder to serve.
        #[arg(default_value = ".")]
        folder: String,
        /// The name Archee shows; by default the host and folder.
        #[arg(long)]
        name: Option<String>,
    },
    /// Show who is signed in.
    Whoami,
    /// End this sign-in.
    Logout,
    /// Install the latest release over this one.
    Update,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command};

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Adding a command to `--help` needs a stated reason (see AGENTS.md); update this list with it.
    #[test]
    fn help_shows_exactly_these_commands() {
        let command = Cli::command();
        let shown: Vec<&str> = command
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(shown, ["login", "connect", "whoami", "logout", "update"]);
    }

    #[test]
    fn removed_commands_and_flags_are_refused() {
        for args in [
            ["archee", "run", "x"].as_slice(),
            &["archee", "bridge", "poll"],
            &["archee", "--skill"],
            &["archee", "--token", "t", "whoami"],
            &["archee", "--url", "u", "whoami"],
            &["archee", "login", "--timeout", "5"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
        let parsed = Cli::try_parse_from(["archee", "whoami", "--json"]).unwrap();
        assert!(matches!(parsed.command, Some(Command::Whoami)));
    }
}
