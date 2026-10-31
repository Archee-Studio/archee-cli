//! One output contract: compact text by default, lossless JSON with `--json`, errors with a next step.

use std::fmt;

use serde::Serialize;

pub(crate) const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
pub(crate) const EXIT_AUTH: u8 = 3;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CliError {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) next: Option<String>,
    #[serde(skip)]
    pub(crate) exit: u8,
}

impl CliError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            next: None,
            exit: EXIT_ERROR,
        }
    }

    pub(crate) fn next(mut self, step: impl Into<String>) -> Self {
        self.next = Some(step.into());
        self
    }

    pub(crate) fn exit(mut self, code: u8) -> Self {
        self.exit = code;
        self
    }

    pub(crate) fn io(action: &str, error: &std::io::Error) -> Self {
        Self::new("io", format!("{action}: {error}"))
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

#[derive(Debug, Serialize)]
struct Success<'a, T: Serialize> {
    ok: bool,
    data: &'a T,
}

#[derive(Debug, Serialize)]
struct Failure<'a> {
    ok: bool,
    error: &'a CliError,
}

/// Print a result; a failure to serialize it is reported rather than lost.
pub(crate) fn emit<T: Serialize>(as_json: bool, data: &T, text: &str) -> Result<(), CliError> {
    if as_json {
        let line = serde_json::to_string(&Success { ok: true, data })
            .map_err(|e| CliError::new("internal", format!("could not encode the result: {e}")))?;
        println!("{line}");
    } else {
        println!("{text}");
    }
    Ok(())
}

/// Structured errors go to stdout with `--json`; plain ones to stderr.
pub(crate) fn emit_error(as_json: bool, error: &CliError) {
    if as_json {
        let failure = Failure { ok: false, error };
        match serde_json::to_string(&failure) {
            Ok(line) => println!("{line}"),
            Err(_) => println!(
                r#"{{"ok":false,"error":{{"code":"internal","message":"unencodable error","next":null}}}}"#
            ),
        }
        return;
    }
    eprintln!("error: {}", error.message);
    if let Some(next) = &error.next {
        eprintln!("next: {next}");
    }
}
