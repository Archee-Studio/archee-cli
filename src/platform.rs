//! What differs between Unix and Windows: the shell, killing a command, signals, state locations.
//!
//! Each item has a `cfg(unix)` and a `cfg(windows)` body and the rest of the crate stays the same.
//! The Unix bodies are the original code, moved here unchanged.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::output::CliError;

/// Environment a script may see. The worker's own token is never in this list.
#[cfg(unix)]
pub(crate) const SCRIPT_ENV: &[&str] = &["PATH", "HOME", "LANG", "TERM", "USER", "TMPDIR"];
#[cfg(windows)]
pub(crate) const SCRIPT_ENV: &[&str] = &[
    "PATH",
    "SystemRoot",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "COMSPEC",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
    "ProgramFiles",
    "USERNAME",
    "WINDIR",
    "LANG",
    "TERM",
];

/// Windows allows 32,767 UTF-16 units on a command line; the script and its prelude travel on it.
const WINDOWS_SCRIPT_UNITS: usize = 30_000;

/// The PowerShell arguments for one script: no profile, no prompts, UTF-8 out.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the Windows build; tested everywhere")
)]
pub(crate) fn powershell_args(script: &str) -> Vec<String> {
    // The console code page is OEM by default; pipes would otherwise carry mangled text.
    let prelude = "try { $OutputEncoding = [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false } catch {}; $ProgressPreference = 'SilentlyContinue'; ";
    [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
    ]
    .iter()
    .map(|flag| (*flag).to_owned())
    .chain(std::iter::once(format!("{prelude}{script}")))
    .collect()
}

/// Whether a script fits where it must travel.
pub(crate) fn script_fits(script: &str) -> Result<(), String> {
    if cfg!(windows) && command_line_units(script) > WINDOWS_SCRIPT_UNITS {
        return Err(
            "the script is too long for a Windows command line; write it in smaller steps"
                .to_owned(),
        );
    }
    Ok(())
}

/// Worst-case UTF-16 units on the command line: quotes and backslashes may be escaped, so count them twice.
fn command_line_units(script: &str) -> usize {
    script.encode_utf16().count() + script.matches(['"', '\\']).count()
}

/// `/bin/bash -c <script>` in its own process group.
#[cfg(unix)]
pub(crate) fn script_command(script: &str) -> Command {
    let mut command = Command::new("/bin/bash");
    command.args(["-c", script]).process_group(0);
    command
}

/// Windows PowerShell 5.1 ships with Windows, so nothing else is required. `CREATE_NEW_PROCESS_GROUP`
/// keeps the first Ctrl-C in the console from killing the running command: only the worker drains.
#[cfg(windows)]
pub(crate) fn script_command(script: &str) -> Command {
    let mut command = Command::new("powershell");
    command
        .args(powershell_args(script))
        .creation_flags(0x0000_0200);
    command
}

/// No `unsafe` and no libc: the system's `kill` signals the process group.
#[cfg(unix)]
pub(crate) async fn kill_tree(pid: u32) {
    let status = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if status.is_err() {
        // /bin/kill is part of every supported system; the child's own kill_on_drop remains.
        eprintln!("warning: could not signal process group {pid}");
    }
}

/// `taskkill /T /F` ends the process and its descendants. A process fully detached from its parent
/// is out of reach of the tree walk, so Stop is slightly weaker here than on Unix.
#[cfg(windows)]
pub(crate) async fn kill_tree(pid: u32) {
    let status = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if status.is_err() {
        eprintln!("warning: could not run taskkill for process {pid}");
    }
}

/// Text from a script's streams: PowerShell may prefix a byte-order mark.
pub(crate) fn clean_output(text: String) -> String {
    if cfg!(windows) {
        text.trim_start_matches('\u{feff}').to_owned()
    } else {
        text
    }
}

#[cfg(unix)]
pub(crate) fn default_state_root() -> Result<PathBuf, CliError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CliError::new("state", "HOME is not set; set ARCHEE_CLI_HOME"))?;
    Ok(PathBuf::from(home).join(".local/state/archee-cli"))
}

#[cfg(windows)]
pub(crate) fn default_state_root() -> Result<PathBuf, CliError> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| CliError::new("state", "LOCALAPPDATA is not set; set ARCHEE_CLI_HOME"))?;
    Ok(PathBuf::from(base).join("archee-cli"))
}

/// Mode 0600 from the first byte. Windows has no such mode: see `restrict_to_owner`.
#[cfg(unix)]
pub(crate) fn private_mode(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600)
}

#[cfg(windows)]
pub(crate) fn private_mode(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    options
}

/// Windows: the owner alone may open the file, wherever `ARCHEE_CLI_HOME` points. Runs on the empty
/// file, before any secret is written, and a failure stops the write.
#[cfg(windows)]
pub(crate) fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", "*S-1-3-4:F"])
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls exited with {status}"
        )))
    }
}

#[cfg(unix)]
pub(crate) fn restrict_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The served folder's identity. Unix: device and inode, so a replaced folder is a different one.
#[cfg(unix)]
pub(crate) fn folder_identity(folder: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let info = std::fs::metadata(folder)?;
    Ok(format!("{}-{}", info.dev(), info.ino()))
}

/// Windows: a digest of the normalized path, so a replaced folder is no longer detected.
#[cfg(windows)]
pub(crate) fn folder_identity(folder: &Path) -> std::io::Result<String> {
    std::fs::metadata(folder)?;
    Ok(crate::paths::path_digest(folder))
}

/// File names that start `program` on this OS.
pub(crate) fn program_files(program: &str) -> Vec<String> {
    if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".com"]
            .iter()
            .map(|extension| format!("{program}{extension}"))
            .collect()
    } else {
        vec![program.to_owned()]
    }
}

/// Whether an entry from a directory scan must not be walked or read: a symlink everywhere, and on
/// Windows also a reparse-point directory, because Rust's `is_symlink` is false for a junction. A
/// reparse-point file stays: OneDrive's on-demand files are ordinary to the person.
#[cfg(unix)]
pub(crate) fn is_link(_entry: &std::fs::DirEntry, kind: &std::fs::FileType) -> bool {
    kind.is_symlink()
}

#[cfg(windows)]
pub(crate) fn is_link(entry: &std::fs::DirEntry, kind: &std::fs::FileType) -> bool {
    use std::os::windows::fs::MetadataExt;
    kind.is_symlink()
        || (kind.is_dir()
            && entry
                .metadata()
                .is_ok_and(|info| crate::paths::reparse_flag(info.file_attributes())))
}

/// Termination requests: SIGTERM and SIGINT, or Ctrl-C and the console closing.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub(crate) fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct Signals {
    interrupt: tokio::signal::windows::CtrlC,
    close: tokio::signal::windows::CtrlClose,
}

#[cfg(windows)]
impl Signals {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
            close: tokio::signal::windows::ctrl_close()?,
        })
    }

    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.close.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{command_line_units, powershell_args, program_files, script_fits};

    #[test]
    fn powershell_runs_without_profile_prompts_or_policy() {
        let args = powershell_args("Get-Date; echo 'é'");
        assert_eq!(
            &args[..5],
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
        assert!(args[5].contains("UTF8Encoding $false"));
        assert!(args[5].ends_with("Get-Date; echo 'é'"));
    }

    #[test]
    fn a_script_too_long_for_windows_is_refused_only_there() {
        let long = "x".repeat(40_000);
        assert_eq!(script_fits(&long).is_err(), cfg!(windows));
        assert!(script_fits("echo hi").is_ok());
    }

    #[test]
    fn the_windows_limit_counts_utf16_units_and_escapes() {
        assert_eq!(command_line_units("ab"), 2);
        assert_eq!(command_line_units("😀"), 2);
        assert_eq!(command_line_units(r#"a"\"#), 5);
    }

    #[test]
    fn program_names_follow_the_os() {
        let names = program_files("agentcad");
        assert_eq!(names.contains(&"agentcad.exe".to_owned()), cfg!(windows));
        assert!(names.iter().all(|n| n.starts_with("agentcad")));
    }
}
