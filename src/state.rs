//! Local state, outside any served folder: the sign-in, worker credentials and journals.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::{CliError, EXIT_USAGE};
use crate::paths::contained;
use crate::platform;

const STATE_ENV: &str = "ARCHEE_CLI_HOME";

/// The signed-in person, on this machine; the current project and what the next run attaches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct UserSession {
    pub(crate) url: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) pending_uploads: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredCredential {
    pub(crate) base_url: String,
    pub(crate) worker_token: String,
    pub(crate) outpost_id: String,
    /// The folder this outpost serves. Its chats' commands must never reach another folder.
    #[serde(default)]
    pub(crate) folder: Option<String>,
}

pub(crate) fn state_root() -> Result<PathBuf, CliError> {
    if let Some(value) = std::env::var_os(STATE_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    platform::default_state_root()
}

/// Written 0600 from the first byte (Windows: an owner-only ACL before any byte).
/// A rename keeps a reader from seeing half a file.
fn write_private(path: &Path, data: &[u8]) -> Result<(), CliError> {
    let fail = |e: std::io::Error| CliError::io(&format!("could not write {}", path.display()), &e);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(fail)?;
    }
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    let mut file = platform::private_mode(options.write(true).create_new(true))
        .open(&temp)
        .map_err(fail)?;
    platform::restrict_to_owner(&temp).map_err(fail)?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(fail)?;
    fs::rename(&temp, path).map_err(fail)
}

fn session_path() -> Result<PathBuf, CliError> {
    Ok(state_root()?.join("session.json"))
}

pub(crate) fn load_session() -> Result<UserSession, CliError> {
    let path = session_path()?;
    match fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| {
            CliError::new("state", format!("{} is unreadable: {e}", path.display()))
                .next(format!("delete {} and run `archee login`", path.display()))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UserSession::default()),
        Err(e) => Err(CliError::io("could not read the sign-in", &e)),
    }
}

pub(crate) fn save_session(session: &UserSession) -> Result<(), CliError> {
    let raw = serde_json::to_vec(session)
        .map_err(|e| CliError::new("internal", format!("could not encode the sign-in: {e}")))?;
    write_private(&session_path()?, &raw)
}

pub(crate) fn clear_session() -> Result<(), CliError> {
    match fs::remove_file(session_path()?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(CliError::io("could not remove the sign-in", &e))
        }
        _ => Ok(()),
    }
}

/// The worker's own state must never live inside the folder it serves.
pub(crate) fn ensure_outside(workdir: &Path, state: &Path) -> Result<(), CliError> {
    if contained(workdir, state, cfg!(windows)) {
        return Err(CliError::new(
            "state",
            format!(
                "state directory {} is inside the served folder",
                state.display()
            ),
        )
        .next("point ARCHEE_CLI_HOME elsewhere"));
    }
    Ok(())
}

/// One stable id per outpost on this machine: the server lets an id serve one outpost only.
pub(crate) fn instance_id(outpost: &str) -> Result<String, CliError> {
    let marker = state_root()?.join("instances").join(outpost);
    if let Ok(existing) = fs::read_to_string(&marker) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_owned());
        }
    }
    let fresh = uuid::Uuid::new_v4().simple().to_string();
    write_private(&marker, fresh.as_bytes())?;
    Ok(fresh)
}

fn credential_path(outpost: &str) -> Result<PathBuf, CliError> {
    Ok(state_root()?
        .join("credentials")
        .join(format!("{outpost}.json")))
}

pub(crate) fn save_credential(
    outpost: &str,
    credential: &StoredCredential,
) -> Result<(), CliError> {
    let raw = serde_json::to_vec(credential)
        .map_err(|e| CliError::new("internal", format!("could not encode the credential: {e}")))?;
    write_private(&credential_path(outpost)?, &raw)
}

impl StoredCredential {
    /// A chat bound to this outpost must stay in its folder, so no other folder may take over.
    pub(crate) fn ensure_serves(&self, outpost: &str, folder: &Path) -> Result<(), CliError> {
        match self.folder.as_deref() {
            Some(own) if !same_folder(Path::new(own), folder) => Err(CliError::new(
                "usage",
                format!("outpost {outpost} serves {own}, not {}", folder.display()),
            )
            .next("use another outpost name for this folder")
            .exit(EXIT_USAGE)),
            _ => Ok(()),
        }
    }
}

pub(crate) fn load_credential(outpost: &str) -> Result<StoredCredential, CliError> {
    let raw = fs::read(credential_path(outpost)?).map_err(|_| {
        CliError::new(
            "no_credential",
            format!("no stored credential for outpost {outpost}"),
        )
        .next("archee connect")
    })?;
    serde_json::from_slice(&raw)
        .map_err(|e| CliError::new("state", format!("stored credential is unusable: {e}")))
}

/// Windows folder names ignore letter case.
fn same_folder(left: &Path, right: &Path) -> bool {
    contained(left, right, cfg!(windows)) && contained(right, left, cfg!(windows))
}

/// The served folder's identity: see `platform::folder_identity`.
pub(crate) fn workdir_id(folder: &Path) -> Result<String, CliError> {
    platform::folder_identity(folder).map_err(|e| CliError::io("could not read the folder", &e))
}

#[cfg(test)]
mod tests {
    /// A file under a shared root must not inherit that root's ACL.
    #[cfg(windows)]
    #[test]
    fn a_private_file_keeps_no_inherited_access_on_windows() {
        let dir =
            std::env::temp_dir().join(format!("archee-acl-{}", uuid::Uuid::new_v4().simple()));
        let file = dir.join("credential.json");
        super::write_private(&file, b"secret").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"secret");
        let acl = std::process::Command::new("icacls")
            .arg(&file)
            .output()
            .unwrap();
        let acl = String::from_utf8_lossy(&acl.stdout).into_owned();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!acl.contains("(I)"), "inherited access remains: {acl}");
    }

    use std::path::Path;

    use super::StoredCredential;

    fn serving(folder: Option<&str>) -> StoredCredential {
        StoredCredential {
            base_url: "http://localhost".to_owned(),
            worker_token: "token".to_owned(),
            outpost_id: "outpost".to_owned(),
            folder: folder.map(str::to_owned),
        }
    }

    #[test]
    fn an_enrolled_outpost_serves_only_its_folder() {
        let credential = serving(Some("/projects/a"));
        assert!(
            credential
                .ensure_serves("laptop", Path::new("/projects/a"))
                .is_ok()
        );
        let refused = credential
            .ensure_serves("laptop", Path::new("/projects/b"))
            .unwrap_err();
        assert_eq!(refused.exit, crate::output::EXIT_USAGE);
    }

    #[test]
    fn a_credential_from_before_connect_has_no_folder_to_enforce() {
        assert!(
            serving(None)
                .ensure_serves("laptop", Path::new("/any"))
                .is_ok()
        );
    }
}
