//! The commands for a person: sign in, who am I, sign out, and the home view.

use std::io::IsTerminal;
use std::time::Duration;

use crate::api::Client;
use crate::cli::{Cli, Command};
use crate::connect;
use crate::output::{CliError, EXIT_AUTH, emit};
use crate::state::{UserSession, clear_session, load_session, save_session};
use crate::update;

const DEFAULT_URL: &str = "https://app.archee.studio";
/// How long a sign-in may wait for the person to approve it in the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);

/// The server to talk to: `ARCHEE_URL`, else the stored sign-in's, else the default.
pub(crate) fn base_url(session: &UserSession) -> String {
    env_value("ARCHEE_URL")
        .or_else(|| session.url.clone())
        .unwrap_or_else(|| DEFAULT_URL.to_owned())
}

/// An access token from `ARCHEE_TOKEN`; there is no flag, since arguments show in `ps`.
pub(crate) fn env_token() -> Option<String> {
    env_value("ARCHEE_TOKEN")
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn signed_in(session: &UserSession) -> Result<Client, CliError> {
    let token = env_token()
        .or_else(|| session.token.clone())
        .ok_or_else(|| {
            CliError::new("unauthorized", "you are not signed in")
                .next("archee login")
                .exit(EXIT_AUTH)
        })?;
    Client::new(&base_url(session), Some(&token))
}

/// Read-only home view; it makes no network call.
pub(crate) fn home(cli: &Cli) -> Result<(), CliError> {
    let session = load_session()?;
    let url = base_url(&session);
    let signed = env_token().is_some() || session.token.is_some();
    let next = if signed {
        "archee connect FOLDER"
    } else {
        "archee login"
    };
    let text = format!(
        "archee {}\nserver {url}\nsigned in: {}\nnext: {next}",
        crate::api::CLI_VERSION,
        if signed { "yes" } else { "no" },
    );
    let data = serde_json::json!({"url": url, "signed_in": signed});
    emit(cli.json, &data, &text)
}

/// A device-code sign-in: prints the code and URL, polls, and returns the token unsaved.
pub(crate) async fn device_token(url: &str) -> Result<String, CliError> {
    let client = Client::new(url, None)?;
    let started = client.login_start().await?;
    eprintln!("Open {}", started.verification_url);
    eprintln!("and check that it shows this code: {}", started.user_code);
    if std::io::stdout().is_terminal() {
        open_browser(&started.verification_url);
    }
    let wait = async {
        loop {
            tokio::time::sleep(Duration::from_secs(started.interval.max(1))).await;
            let polled = client.login_poll(&started.device_code).await?;
            if let Some(token) = polled.token {
                return Ok::<String, CliError>(token);
            }
        }
    };
    tokio::time::timeout(SIGN_IN_TIMEOUT, wait)
        .await
        .map_err(|_| {
            CliError::new("timeout", "the sign-in was not approved in time").next("archee login")
        })?
}

async fn login(cli: &Cli) -> Result<(), CliError> {
    let mut session = load_session()?;
    let url = base_url(&session);
    let token = match env_token() {
        Some(token) => token,
        None => device_token(&url).await?,
    };
    let user = Client::new(&url, Some(&token))?.me().await?;
    session.url = Some(url);
    session.token = Some(token);
    save_session(&session)?;
    emit(
        cli.json,
        &user,
        &format!("signed in as {} <{}>", user.name, user.email),
    )
}

/// A convenience only: the URL is already printed, so a failure to open changes nothing.
/// The server names it, so only a web link reaches the system's opener.
fn open_browser(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _spawned = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn logout(cli: &Cli) -> Result<(), CliError> {
    let session = load_session()?;
    if let Some(token) = session.token.as_deref() {
        // Local sign-out must work offline; a token that did not revoke is reported.
        let ended = match Client::new(&base_url(&session), Some(token)) {
            Ok(client) => client.logout().await.map_err(CliError::from),
            Err(error) => Err(error),
        };
        if let Err(error) = ended {
            eprintln!("warning: the server did not end this sign-in: {error}");
        }
    }
    clear_session()?;
    emit(
        cli.json,
        &serde_json::json!({"signed_in": false}),
        "signed out",
    )
}

pub(crate) async fn dispatch(cli: &Cli, command: &Command) -> Result<(), CliError> {
    match command {
        Command::Login => login(cli).await,
        Command::Logout => logout(cli).await,
        Command::Whoami => {
            let user = signed_in(&load_session()?)?.me().await?;
            emit(cli.json, &user, &format!("{} <{}>", user.name, user.email))
        }
        Command::Connect { folder, name } => connect::connect(folder, name.as_deref()).await,
        Command::Update => update::run(cli).await,
    }
}
