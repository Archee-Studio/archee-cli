//! Ported from the Python worker's `test_operations.py`: the same confinement and bounds.

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::models::{OperationPayload, OperationResult};
use crate::operations::{OperationFailure, execute, fnmatch, resolve};

fn payload(json: &str) -> OperationPayload {
    serde_json::from_str(json).unwrap()
}

async fn file(
    root: &Path,
    action: &str,
    path: &str,
    extra: &str,
) -> Result<OperationResult, OperationFailure> {
    let (_tx, rx) = watch::channel(false);
    let body = format!(
        r#"{{"kind":"file_operation","file_operation":{{"action":"{action}","path":"{path}"{extra}}}}}"#
    );
    execute(root, &payload(&body), rx).await
}

async fn script(root: &Path, source: &str, timeout: u64) -> OperationResult {
    let (_tx, rx) = watch::channel(false);
    let body =
        serde_json::json!({"kind":"script","script":{"script":source,"timeout_seconds":timeout}})
            .to_string();
    execute(root, &payload(&body), rx).await.unwrap()
}

#[test]
fn resolve_confines_paths() {
    let dir = tempfile::tempdir().unwrap();
    assert!(resolve(dir.path(), "a/b.txt").is_ok());
    for bad in ["", "/etc/passwd", "../x", "a/../../x", "a\0b"] {
        assert!(resolve(dir.path(), bad).is_err(), "{bad:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_cannot_lead_out() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/etc", dir.path().join("out")).unwrap();
    assert!(resolve(dir.path(), "out/passwd").is_err());
    assert!(resolve(dir.path(), "out/new.txt").is_err());
    std::os::unix::fs::symlink("/nonexistent/x", dir.path().join("dangling")).unwrap();
    assert!(resolve(dir.path(), "dangling").is_err());
}

#[tokio::test]
async fn read_write_append_rename_delete_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let wrote = file(root, "write", "d/a.txt", r#","content":"hello""#)
        .await
        .unwrap();
    assert_eq!(
        wrote,
        OperationResult::Write {
            path: "d/a.txt".into(),
            bytes_written: 5
        }
    );
    file(root, "append", "d/a.txt", r#","content":" you""#)
        .await
        .unwrap();
    let OperationResult::Read {
        content,
        size_bytes,
        truncated,
        ..
    } = file(root, "read", "d/a.txt", "").await.unwrap()
    else {
        panic!("not a read");
    };
    assert_eq!(
        (content.as_str(), size_bytes, truncated),
        ("hello you", 9, false)
    );
    file(root, "rename", "d/a.txt", r#","to_path":"e/b.txt""#)
        .await
        .unwrap();
    assert!(file(root, "read", "d/a.txt", "").await.is_err());
    file(root, "delete", "e/b.txt", "").await.unwrap();
    assert!(!root.join("e/b.txt").exists());
}

#[tokio::test]
async fn list_glob_and_grep_skip_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("d")).unwrap();
    std::fs::write(root.join("d/a.dxf"), "needle\nhay\n").unwrap();
    std::fs::write(root.join("b.txt"), "hay\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", root.join("link.dxf")).unwrap();
    let OperationResult::Glob { matches } = file(root, "glob", ".", r#","pattern":"*.dxf""#)
        .await
        .unwrap()
    else {
        panic!("not a glob");
    };
    assert_eq!(matches, ["d/a.dxf"]);
    let OperationResult::Grep { matches } = file(root, "grep", ".", r#","pattern":"needle""#)
        .await
        .unwrap()
    else {
        panic!("not a grep");
    };
    assert_eq!((matches[0].path.as_str(), matches[0].line), ("d/a.dxf", 1));
    assert_eq!(matches.len(), 1);
    let OperationResult::List { entries, .. } = file(root, "list", "d", "").await.unwrap() else {
        panic!("not a list");
    };
    assert_eq!(entries[0].name, "a.dxf");
}

#[cfg(unix)]
#[tokio::test]
async fn a_script_reports_streams_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let OperationResult::Script {
        exit_code,
        stdout,
        stderr,
        timed_out,
    } = script(dir.path(), "pwd; echo oops >&2; exit 3", 30).await
    else {
        panic!("not a script");
    };
    assert_eq!((exit_code, timed_out), (3, false));
    assert!(
        stdout
            .trim_end()
            .ends_with(dir.path().file_name().unwrap().to_str().unwrap())
    );
    assert_eq!(stderr, "oops\n");
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let OperationResult::Script { timed_out, .. } =
        script(dir.path(), "sleep 60 & sleep 60", 1).await
    else {
        panic!("not a script");
    };
    assert!(timed_out);
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[cfg(unix)]
#[tokio::test]
async fn the_worker_token_never_reaches_a_script() {
    let dir = tempfile::tempdir().unwrap();
    let OperationResult::Script { stdout, .. } = script(dir.path(), "env | cut -d= -f1", 30).await
    else {
        panic!("not a script");
    };
    let allowed = [
        "PATH",
        "HOME",
        "LANG",
        "TERM",
        "USER",
        "TMPDIR",
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
        super::operations::ON_COMPUTER_ENV,
    ];
    for name in stdout.lines() {
        assert!(allowed.contains(&name), "{name} leaked");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn noisy_output_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let OperationResult::Script { stdout, .. } =
        script(dir.path(), "yes x | head -c 1000000", 30).await
    else {
        panic!("not a script");
    };
    assert!(stdout.starts_with("[truncated]\n"));
    assert!(stdout.len() < 70_000);
}

#[cfg(unix)]
#[tokio::test]
async fn a_cancel_kills_a_running_script() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = watch::channel(false);
    let body = r#"{"kind":"script","script":{"script":"sleep 60","timeout_seconds":120}}"#;
    let parsed = payload(body);
    let root = dir.path().to_owned();
    let task = tokio::spawn(async move { execute(&root, &parsed, rx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    tx.send(true).unwrap();
    let OperationResult::Script { timed_out, .. } = task.await.unwrap().unwrap() else {
        panic!("not a script");
    };
    assert!(!timed_out);
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_payload_must_carry_its_request_body() {
    assert!(serde_json::from_str::<OperationPayload>(r#"{"kind":"script"}"#).is_err());
    assert!(
        serde_json::from_str::<OperationPayload>(
            r#"{"kind":"script","file_operation":{"action":"read","path":"a"}}"#
        )
        .is_err()
    );
}

#[test]
fn fnmatch_matches_the_python_reference() {
    // Each expectation was taken from Python's fnmatch.fnmatchcase.
    for (pattern, name, want) in [
        ("*.dxf", "a.dxf", true),
        ("*.dxf", "a.dwg", false),
        ("a?c", "abc", true),
        ("a?c", "ac", false),
        ("[ab]x", "bx", true),
        ("[!ab]x", "bx", false),
        ("[a-c]x", "bx", true),
        ("[a", "[a", true),
        ("*", "", true),
    ] {
        assert_eq!(fnmatch(pattern, name), want, "{pattern} {name}");
    }
}

/// Windows: the shell is PowerShell, killing is `taskkill`, and confinement has more to refuse.
#[cfg(windows)]
mod windows {
    use super::*;

    async fn sh(source: &str, timeout: u64) -> OperationResult {
        script(&std::env::temp_dir(), source, timeout).await
    }

    #[test]
    fn resolve_refuses_every_windows_way_out() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "C:x",
            "C:\\x",
            "\\\\server\\share\\x",
            "\\\\?\\C:\\x",
            "..\\x",
            "a\\..\\..\\x",
            "a/..\\x",
            "a.txt:stream",
            "CON",
            "nul.txt",
            "COM1",
            "a.",
            "a ",
        ] {
            assert!(resolve(dir.path(), bad).is_err(), "{bad:?}");
        }
        std::fs::create_dir(dir.path().join("Sub")).unwrap();
        let found = resolve(dir.path(), "sub\\a.txt").unwrap();
        assert!(found.starts_with(dunce::canonicalize(dir.path()).unwrap()));
    }

    #[test]
    fn the_served_folder_has_no_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve(dir.path(), "a.txt").unwrap();
        assert!(
            !resolved.to_string_lossy().starts_with("\\\\?\\"),
            "{resolved:?}"
        );
    }

    #[tokio::test]
    async fn a_junction_is_neither_walked_nor_followed() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let junction = dir.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        std::fs::write(dir.path().join("own.txt"), "needle\n").unwrap();
        let root = dir.path();
        let OperationResult::Glob { matches } = file(root, "glob", ".", r#","pattern":"*.txt""#)
            .await
            .unwrap()
        else {
            panic!("not a glob");
        };
        assert_eq!(matches, ["own.txt"]);
        let OperationResult::Grep { matches } = file(root, "grep", ".", r#","pattern":"needle""#)
            .await
            .unwrap()
        else {
            panic!("not a grep");
        };
        assert_eq!(matches.len(), 1);
        assert!(resolve(root, "link/secret.txt").is_err());
        assert!(file(root, "read", "link/secret.txt", "").await.is_err());
    }

    #[tokio::test]
    async fn a_script_runs_in_the_folder_with_its_streams_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let OperationResult::Script {
            exit_code,
            stdout,
            stderr,
            timed_out,
        } = script(
            dir.path(),
            "(Get-Location).Path; [Console]::Error.WriteLine('oops'); exit 3",
            60,
        )
        .await
        else {
            panic!("not a script");
        };
        assert_eq!((exit_code, timed_out), (3, false));
        let leaf = dir.path().file_name().unwrap().to_str().unwrap();
        assert!(stdout.trim_end().ends_with(leaf), "{stdout:?}");
        assert!(stderr.contains("oops"), "{stderr:?}");
    }

    #[tokio::test]
    async fn text_survives_the_round_trip_as_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let OperationResult::Script { stdout, .. } =
            script(dir.path(), "Write-Output 'Đồ án — ✓'", 60).await
        else {
            panic!("not a script");
        };
        assert_eq!(stdout.trim_end(), "Đồ án — ✓");
        file(
            dir.path(),
            "write",
            "bản vẽ.txt",
            r#","content":"Tiếng Việt ✓""#,
        )
        .await
        .unwrap();
        let OperationResult::Script { stdout, .. } = script(
            dir.path(),
            "Get-Content -Raw -Encoding UTF8 -LiteralPath 'bản vẽ.txt'",
            60,
        )
        .await
        else {
            panic!("not a script");
        };
        assert_eq!(stdout.trim_end(), "Tiếng Việt ✓");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_its_children() {
        let started = Instant::now();
        let OperationResult::Script { timed_out, .. } = sh(
            "powershell -NoProfile -Command Start-Sleep 60; Start-Sleep 60",
            2,
        )
        .await
        else {
            panic!("not a script");
        };
        assert!(timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_cancel_kills_a_running_script() {
        let (tx, rx) = watch::channel(false);
        let parsed = payload(
            r#"{"kind":"script","script":{"script":"Start-Sleep 60","timeout_seconds":120}}"#,
        );
        let root = std::env::temp_dir();
        let task = tokio::spawn(async move { execute(&root, &parsed, rx).await });
        tokio::time::sleep(Duration::from_secs(2)).await;
        let started = Instant::now();
        tx.send(true).unwrap();
        let OperationResult::Script { timed_out, .. } = task.await.unwrap().unwrap() else {
            panic!("not a script");
        };
        assert!(!timed_out);
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn only_the_allowed_environment_reaches_a_script() {
        let OperationResult::Script { stdout, .. } =
            sh("Get-ChildItem Env: | ForEach-Object { $_.Name }", 60).await
        else {
            panic!("not a script");
        };
        let allowed = crate::platform::SCRIPT_ENV;
        // PowerShell adds its own variables to any process it starts.
        let own = [
            "PSModulePath",
            "PSExecutionPolicyPreference",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "CommonProgramFiles",
            "CommonProgramFiles(x86)",
            "CommonProgramW6432",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
            "OS",
            "ALLUSERSPROFILE",
            "ComputerName",
            "COMPUTERNAME",
            "USERDOMAIN",
            "SystemDrive",
            "PROCESSOR_IDENTIFIER",
            "PROCESSOR_LEVEL",
            "PROCESSOR_REVISION",
            "PUBLIC",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOGONSERVER",
            "USERDOMAIN_ROAMINGPROFILE",
            "DriverData",
            "OneDrive",
            "SESSIONNAME",
            super::super::operations::ON_COMPUTER_ENV,
        ];
        for name in stdout.lines().map(str::trim).filter(|n| !n.is_empty()) {
            let known = allowed
                .iter()
                .chain(own.iter())
                .any(|a| a.eq_ignore_ascii_case(name));
            assert!(known, "{name} leaked");
            assert!(!name.to_ascii_lowercase().contains("token"), "{name}");
        }
        assert!(stdout.contains(super::super::operations::ON_COMPUTER_ENV));
    }
}
