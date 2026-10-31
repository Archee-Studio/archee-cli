//! Executing operations inside the served folder.
//!
//! File tools are confined to the folder by resolved paths; shell runs as the worker's OS user and
//! is not a sandbox (approval rules on the server are the boundary for shell).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;

use crate::models::{
    FileAction, FileOperationRequest, GrepMatch, ListEntry, OperationPayload, OperationResult,
    ScriptRequest,
};

/// Set for scripts the agent runs here, so `archee` refuses account commands.
pub(crate) const ON_COMPUTER_ENV: &str = "ARCHEE_ON_COMPUTER";
const MAX_STREAM_BYTES: usize = 64 * 1024;
const DRAIN: Duration = Duration::from_secs(10); // cap on pipes a surviving descendant still holds
const MAX_READ_BYTES: u64 = 1_400_000;
const MAX_FILE_BYTES: usize = 1_400_000;
const MAX_SEARCH_ENTRIES: usize = 10_000;
const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
const GREP_LINE_CHARS: usize = 500;

/// The operation could not run at all; the receipt will be `failed`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OperationFailure(pub(crate) String);

impl From<std::io::Error> for OperationFailure {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T, OperationFailure> {
    Err(OperationFailure(message.into()))
}

/// Confine a path to the folder: no absolute paths, no `..`, no symlink out of it.
pub(crate) fn resolve(workdir: &Path, relative: &str) -> Result<PathBuf, OperationFailure> {
    if relative.is_empty() || relative.contains('\0') {
        return fail("path must be a non-empty relative path");
    }
    let candidate = Path::new(relative);
    let escapes = candidate.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return fail(format!("path escapes the served folder: {relative}"));
    }
    let root = workdir.canonicalize()?;
    // The tail may not exist yet: canonicalize the deepest part that does.
    let full = root.join(candidate);
    let mut existing = full.as_path();
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let resolved = loop {
        match existing.canonicalize() {
            Ok(found) => {
                break missing
                    .iter()
                    .rev()
                    .fold(found, |path, part| path.join(part));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is not missing: its target could be anywhere.
                if existing.is_symlink() {
                    return fail(format!("path escapes the served folder: {relative}"));
                }
                match (existing.file_name(), existing.parent()) {
                    (Some(name), Some(parent)) => {
                        missing.push(name);
                        existing = parent;
                    }
                    _ => return fail(format!("path escapes the served folder: {relative}")),
                }
            }
            Err(error) => return Err(error.into()),
        }
    };
    if !resolved.starts_with(&root) {
        return fail(format!("path escapes the served folder: {relative}"));
    }
    Ok(resolved)
}

/// A fixed-size tail plus the total, so noisy output stays bounded.
async fn read_tail(mut stream: impl AsyncRead + Unpin) -> (Vec<u8>, usize) {
    let mut tail = Vec::new();
    let mut total = 0;
    let mut chunk = vec![0_u8; 64 * 1024];
    while let Ok(count) = stream.read(&mut chunk).await {
        if count == 0 {
            break;
        }
        total += count;
        tail.extend(chunk.iter().take(count));
        if tail.len() > MAX_STREAM_BYTES {
            tail.drain(..tail.len() - MAX_STREAM_BYTES);
        }
    }
    (tail, total)
}

fn stream_text((tail, total): (Vec<u8>, usize)) -> String {
    let body = String::from_utf8_lossy(&tail);
    if total > MAX_STREAM_BYTES {
        format!("[truncated]\n{body}")
    } else {
        body.into_owned()
    }
}

/// No `unsafe` and no libc: the system's `kill` signals the process group.
async fn kill_group(pid: u32) {
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

/// `/bin/bash -c <script>` in its own process group; a timeout or a cancel kills the group.
async fn run_script(
    workdir: &Path,
    request: &ScriptRequest,
    mut cancel: watch::Receiver<bool>,
) -> Result<OperationResult, OperationFailure> {
    let sub = resolve(workdir, &request.working_directory)?;
    if !sub.is_dir() {
        return fail(format!(
            "working directory does not exist: {}",
            request.working_directory
        ));
    }
    let mut command = Command::new("/bin/bash");
    command
        .args(["-c", &request.script])
        .current_dir(&sub)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // Only the basics pass through, so the worker's own token never reaches a script.
    for name in ["PATH", "HOME", "LANG", "TERM", "USER", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env(ON_COMPUTER_ENV, "1");
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| OperationFailure("no stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| OperationFailure("no stderr pipe".into()))?;
    let out_task = tokio::spawn(read_tail(stdout));
    let err_task = tokio::spawn(read_tail(stderr));

    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status.ok(),
        () = tokio::time::sleep(Duration::from_secs(request.timeout_seconds)) => {
            timed_out = true;
            None
        }
        _ = cancel.wait_for(|cancelled| *cancelled) => None,
    };
    let status = match status {
        Some(status) => Some(status),
        None => {
            if let Some(pid) = pid {
                kill_group(pid).await;
            }
            tokio::time::timeout(DRAIN, child.wait())
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    let exit_code = status.and_then(|s| s.code()).unwrap_or(-9);
    // A descendant holding a pipe after the group kill ends at its own pace, never ours.
    let stdout = drained(out_task).await;
    let stderr = drained(err_task).await;
    Ok(OperationResult::Script {
        exit_code,
        timed_out,
        stdout,
        stderr,
    })
}

async fn drained(mut task: tokio::task::JoinHandle<(Vec<u8>, usize)>) -> String {
    match tokio::time::timeout(DRAIN, &mut task).await {
        Ok(Ok(tail)) => stream_text(tail),
        Ok(Err(_)) => String::new(),
        Err(_) => {
            task.abort();
            "[output abandoned: a descendant kept the pipe open]".to_owned()
        }
    }
}

fn regular_file(path: &Path) -> Result<(), OperationFailure> {
    let plain = fs::symlink_metadata(path).is_ok_and(|info| info.file_type().is_file());
    if plain {
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    fail(format!("not a regular file: {name}"))
}

fn ensure_parent(path: &Path) -> Result<(), OperationFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// `*`, `?` and `[...]`, as Python's `fnmatch` on POSIX names.
pub(crate) fn fnmatch(pattern: &str, name: &str) -> bool {
    fn set_match(set: &[char], c: char) -> bool {
        let (negated, set) = match set.split_first() {
            Some(('!', rest)) => (true, rest),
            _ => (false, set),
        };
        let mut found = false;
        let mut items = set.iter().peekable();
        while let Some(&low) = items.next() {
            let mut lookahead = items.clone();
            if lookahead.next() == Some(&'-')
                && let Some(&&high) = lookahead.peek()
            {
                found |= (low..=high).contains(&c);
                items = lookahead;
                items.next();
            } else {
                found |= low == c;
            }
        }
        found != negated
    }
    fn go(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => {
                (0..=name.len()).any(|skip| name.get(skip..).is_some_and(|n| go(rest, n)))
            }
            Some((first, rest)) => {
                let Some((c, tail)) = name.split_first() else {
                    return false;
                };
                if *first == '?' {
                    return go(rest, tail);
                }
                if *first == '['
                    && let Some(end) = rest.iter().skip(1).position(|p| *p == ']').map(|at| at + 1)
                    && let (Some(set), Some(after)) = (rest.get(..end), rest.get(end + 1..))
                {
                    return set_match(set, *c) && go(after, tail);
                }
                first == c && go(rest, tail)
            }
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    go(&pattern, &name)
}

/// Regular files only: a symlink inside the folder may point outside it.
fn bounded_files(workdir: &Path, pattern: Option<&str>) -> Result<Vec<PathBuf>, OperationFailure> {
    let mut files = Vec::new();
    let mut seen = 0_usize;
    let mut stack = vec![workdir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&folder)?.filter_map(Result::ok).collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if pattern.is_some_and(|p| !fnmatch(p, &entry.file_name().to_string_lossy())) {
                continue;
            }
            seen += 1;
            if seen > MAX_SEARCH_ENTRIES {
                return fail("search exceeds the entry cap");
            }
            if !kind.is_symlink() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn read_file(
    workdir: &Path,
    request: &FileOperationRequest,
) -> Result<OperationResult, OperationFailure> {
    let target = resolve(workdir, &request.path)?;
    regular_file(&target)?;
    let file = fs::File::open(&target)?;
    let size_bytes = file.metadata()?.size();
    let mut data = Vec::new();
    file.take(MAX_READ_BYTES + 1).read_to_end(&mut data)?;
    let truncated = size_bytes > MAX_READ_BYTES
        || u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_READ_BYTES;
    data.truncate(usize::try_from(MAX_READ_BYTES).unwrap_or(usize::MAX));
    let content = String::from_utf8_lossy(&data).into_owned();
    Ok(OperationResult::Read {
        path: request.path.clone(),
        size_bytes,
        truncated,
        content,
    })
}

fn write_file(
    workdir: &Path,
    request: &FileOperationRequest,
    append: bool,
) -> Result<OperationResult, OperationFailure> {
    let Some(content) = &request.content else {
        return fail("write needs string content");
    };
    if content.len() > MAX_FILE_BYTES {
        return fail("content exceeds the size cap");
    }
    let target = resolve(workdir, &request.path)?;
    ensure_parent(&target)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(append)
        .write(true)
        .truncate(!append)
        .open(&target)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(OperationResult::Write {
        path: request.path.clone(),
        bytes_written: content.len(),
    })
}

fn grep(
    workdir: &Path,
    request: &FileOperationRequest,
) -> Result<OperationResult, OperationFailure> {
    let Some(pattern) = request.pattern.as_deref().filter(|p| !p.is_empty()) else {
        return fail("grep needs a pattern");
    };
    let root = workdir.canonicalize()?;
    let mut matches = Vec::new();
    let mut seen = 0_u64;
    for path in bounded_files(&root, None)? {
        let size = fs::metadata(&path)?.size();
        if size > MAX_SEARCH_BYTES.saturating_sub(seen) {
            continue;
        }
        let Ok(raw) = fs::read(&path) else { continue };
        seen = seen.saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
        let text = String::from_utf8_lossy(&raw);
        for (index, line) in text.lines().enumerate() {
            if line.contains(pattern) {
                matches.push(GrepMatch {
                    path: relative_to(&root, &path),
                    line: index + 1,
                    text: line.chars().take(GREP_LINE_CHARS).collect(),
                });
                if matches.len() >= request.max_entries {
                    return Ok(OperationResult::Grep { matches });
                }
            }
        }
    }
    Ok(OperationResult::Grep { matches })
}

fn run_file_operation(
    workdir: &Path,
    request: &FileOperationRequest,
) -> Result<OperationResult, OperationFailure> {
    match request.action {
        FileAction::Read => read_file(workdir, request),
        FileAction::Write => write_file(workdir, request, false),
        FileAction::Append => write_file(workdir, request, true),
        FileAction::Delete => {
            let target = resolve(workdir, &request.path)?;
            regular_file(&target)?;
            fs::remove_file(&target)?;
            Ok(OperationResult::Delete {
                deleted: request.path.clone(),
            })
        }
        FileAction::Rename => {
            let target = resolve(workdir, &request.path)?;
            regular_file(&target)?;
            let Some(to_path) = &request.to_path else {
                return fail("rename needs to_path");
            };
            let moved = resolve(workdir, to_path)?;
            ensure_parent(&moved)?;
            fs::rename(&target, &moved)?;
            Ok(OperationResult::Rename {
                path: request.path.clone(),
                to_path: to_path.clone(),
            })
        }
        FileAction::List => {
            let target = resolve(workdir, &request.path)?;
            if !target.is_dir() {
                return fail(format!("not a directory: {}", request.path));
            }
            let mut items: Vec<_> = fs::read_dir(&target)?.filter_map(Result::ok).collect();
            items.sort_by_key(fs::DirEntry::file_name);
            let entries = items
                .iter()
                .take(request.max_entries)
                .map(|item| ListEntry {
                    name: item.file_name().to_string_lossy().into_owned(),
                    kind: (if item.path().is_dir() { "dir" } else { "file" }).to_owned(),
                })
                .collect();
            Ok(OperationResult::List {
                path: request.path.clone(),
                entries,
            })
        }
        FileAction::Glob => {
            let root = workdir.canonicalize()?;
            let pattern = request.pattern.as_deref().unwrap_or("*");
            let matches = bounded_files(&root, Some(pattern))?
                .iter()
                .take(request.max_entries)
                .map(|path| relative_to(&root, path))
                .collect();
            Ok(OperationResult::Glob { matches })
        }
        FileAction::Grep => grep(workdir, request),
    }
}

/// Dispatch one validated operation; failures become `failed` receipts, never crashes.
pub(crate) async fn execute(
    workdir: &Path,
    payload: &OperationPayload,
    cancel: watch::Receiver<bool>,
) -> Result<OperationResult, OperationFailure> {
    match payload {
        OperationPayload::Script(request) => run_script(workdir, request, cancel).await,
        OperationPayload::File(_) if *cancel.borrow() => {
            fail("operation aborted: the lease was lost")
        }
        OperationPayload::File(request) => {
            let (root, owned) = (workdir.to_path_buf(), request.clone());
            tokio::task::spawn_blocking(move || run_file_operation(&root, &owned))
                .await
                .map_err(|e| OperationFailure(format!("the file operation did not finish: {e}")))?
        }
    }
}
