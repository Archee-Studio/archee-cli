//! `archee connect`: one command that lets the agent work in this folder.
//!
//! Everything here composes existing steps: sign-in, outpost, one-use reference, worker. The sign-in
//! it makes for itself never reaches the disk, because the commands the agent runs here can read the disk.

use std::path::Path;

use crate::api::{ApiError, Client};
use crate::output::{CliError, EXIT_AUTH, EXIT_USAGE};
use crate::state::{load_credential, load_session};
use crate::users::{base_url, device_token, env_token};
use crate::worker;

const NAME_CHARS: usize = 80;
const HOST_CHARS: usize = 40;

fn clean(text: &str) -> String {
    let kept: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    kept.trim_matches(|c| matches!(c, '-' | '.')).to_owned()
}

/// `<host>-<folder>`: two folders never share an outpost, since its operations may reach any worker.
fn default_name(host: &str, folder: &Path) -> String {
    let host = clean(host.split('.').next().unwrap_or_default());
    let host: String = host.chars().take(HOST_CHARS).collect();
    let folder = clean(&folder.file_name().unwrap_or_default().to_string_lossy());
    let name = match (host.is_empty(), folder.is_empty()) {
        (false, false) => format!("{host}-{folder}"),
        (false, true) => host,
        (true, false) => folder,
        (true, true) => "computer".to_owned(),
    };
    name.chars().take(NAME_CHARS).collect()
}

fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

/// The account token to enrol with, and whether it is ours to end once enrolled.
async fn account_token(url: &str) -> Result<(String, bool), CliError> {
    if let Some(token) = env_token() {
        return Ok((token, false));
    }
    if let Some(token) = load_session()?.token {
        match Client::new(url, Some(&token))?.me().await {
            Ok(_) => return Ok((token, false)),
            Err(ApiError::Status { status: 401, .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((device_token(url).await?, true))
}

async fn enroll(url: &str, name: &str, folder: &Path) -> Result<(), CliError> {
    let (token, ours) = account_token(url).await?;
    let client = Client::new(url, Some(&token))?;
    let enrolled = async {
        let outposts = client.list_outposts().await?;
        let id = match outposts.into_iter().find(|o| o.name == name) {
            Some(found) => found.id,
            None => client.create_outpost(name).await?.id,
        };
        let issued = client.issue_registration(&id).await?;
        worker::enroll(url, name, &issued.reference, folder).await?;
        Ok::<(), CliError>(())
    }
    .await;
    if ours && let Err(error) = client.logout().await {
        eprintln!(
            "warning: the server did not end the sign-in used to connect: {}",
            CliError::from(error)
        );
    }
    enrolled
}

enum Fit {
    Stored,
    Free,
    OtherFolder,
}

/// Whether this name can serve this folder: a chat bound to an outpost must stay in its folder.
fn usable(name: &str, url: &str, workdir: &str) -> Fit {
    match load_credential(name) {
        Ok(credential) if credential.folder.as_deref().is_some_and(|f| f != workdir) => {
            Fit::OtherFolder
        }
        // No folder recorded: from before `connect`, so enrol again and record this one.
        Ok(credential) if credential.base_url == url && credential.folder.is_some() => Fit::Stored,
        _ => Fit::Free,
    }
}

/// Two folders with one basename get `name`, then `name-2`, and each keeps its own.
fn free_name(base: &str, url: &str, workdir: &str) -> String {
    let stem: String = base.chars().take(NAME_CHARS - 4).collect();
    std::iter::once(base.to_owned())
        .chain((2..).map(|n| format!("{stem}-{n}")))
        .find(|name| !matches!(usable(name, url, workdir), Fit::OtherFolder))
        .unwrap_or_else(|| base.to_owned())
}

pub(crate) async fn connect(folder: &str, name: Option<&str>) -> Result<(), CliError> {
    let served = worker::served_folder(folder)?;
    let session = load_session()?;
    let url = base_url(&session);
    let workdir = served.to_string_lossy().into_owned();
    let name = match name {
        Some(name) => {
            let name = clean(name);
            if name.is_empty() {
                return Err(CliError::new(
                    "usage",
                    "--name needs at least one letter or digit from A-Z, a-z or 0-9",
                )
                .exit(EXIT_USAGE));
            }
            if matches!(usable(&name, &url, &workdir), Fit::OtherFolder) {
                return Err(CliError::new(
                    "name_taken",
                    format!("{name} already serves another folder on this computer"),
                )
                .next("choose another --name")
                .exit(EXIT_USAGE));
            }
            name
        }
        None => free_name(&default_name(&host_name(), &served), &url, &workdir),
    };
    let stored = matches!(usable(&name, &url, &workdir), Fit::Stored);
    if !stored {
        enroll(&url, &name, &served).await?;
    }
    eprintln!("Connected {name}, serving {workdir}");
    eprintln!("Archee runs commands here as you, not in a sandbox. Ctrl-C disconnects.");
    if !on_path("agentcad") {
        eprintln!("note: agentcad is not on PATH, so Archee has only your shell's tools here");
    }
    if session.token.is_some() {
        eprintln!(
            "warning: a saved `archee login` can be read by commands Archee runs; `archee logout` removes it"
        );
    }
    let served = worker::start(&name, &workdir).await;
    match served {
        // A stored credential that was revoked: enrol once more, then serve.
        Err(error) if stored && error.exit == EXIT_AUTH => {
            eprintln!("this computer's credential is no longer valid; connecting it again");
            enroll(&url, &name, Path::new(&workdir)).await?;
            worker::start(&name, &workdir).await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{clean, default_name, free_name};

    #[test]
    fn the_name_is_host_and_folder_within_the_server_pattern() {
        assert_eq!(
            default_name("Sams-Laptop.local", Path::new("/a/My Project")),
            "sams-laptop-my-project"
        );
        assert_eq!(default_name("", Path::new("/")), "computer");
        assert_eq!(default_name("host", Path::new("/")), "host");
        assert_eq!(clean("--Đồ án!--"), "n");
        assert_eq!(free_name("h-app", "u", "/a/app"), "h-app");
        let long = default_name(&"h".repeat(200), Path::new(&"f".repeat(200)));
        assert_eq!(long.chars().count(), 80);
        assert!(long.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
