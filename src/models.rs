//! Typed server responses. The outpost shapes follow the server's worker protocol; the CLI
//! shares no code with the server, so these types are the whole agreement.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) email: String,
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LoginStarted {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    pub(crate) verification_url: String,
    pub(crate) expires_in: u64,
    pub(crate) interval: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LoginPoll {
    pub(crate) status: String,
    pub(crate) token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OutpostView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) online: bool,
    pub(crate) worker_count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RegistrationIssued {
    pub(crate) reference: String,
    pub(crate) expires_at: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeResponse {
    pub(crate) worker_token: String,
    pub(crate) outpost_id: String,
    pub(crate) outpost_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClaimedAssignment {
    pub(crate) assignment_id: String,
    pub(crate) fence_token: i64,
    pub(crate) lease_token: String,
    pub(crate) expires_at: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssignmentListing {
    pub(crate) assignment_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OperationAck {
    pub(crate) receipt_status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptRequest {
    pub(crate) script: String,
    #[serde(default = "here")]
    pub(crate) working_directory: String,
    #[serde(default = "default_timeout")]
    pub(crate) timeout_seconds: u64,
}

fn here() -> String {
    ".".to_owned()
}

fn default_timeout() -> u64 {
    300
}

fn default_entries() -> usize {
    1000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileAction {
    Read,
    Write,
    Append,
    Delete,
    Rename,
    List,
    Glob,
    Grep,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileOperationRequest {
    pub(crate) action: FileAction,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) to_path: Option<String>,
    #[serde(default)]
    pub(crate) pattern: Option<String>,
    #[serde(default = "default_entries")]
    pub(crate) max_entries: usize,
}

/// `kind` must name the one request present, as the Python model required.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawPayload")]
pub(crate) enum OperationPayload {
    Script(ScriptRequest),
    File(FileOperationRequest),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    kind: String,
    #[serde(default)]
    script: Option<ScriptRequest>,
    #[serde(default)]
    file_operation: Option<FileOperationRequest>,
}

impl TryFrom<RawPayload> for OperationPayload {
    type Error = String;

    fn try_from(raw: RawPayload) -> Result<Self, String> {
        match (raw.kind.as_str(), raw.script, raw.file_operation) {
            ("script", Some(script), None) => Ok(Self::Script(script)),
            ("file_operation", None, Some(file)) => Ok(Self::File(file)),
            (kind, _, _) => Err(format!("kind {kind} needs exactly one {kind} request")),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct OperationEnvelope {
    pub(crate) operation_id: String,
    pub(crate) seq: i64,
    pub(crate) payload: OperationPayload,
}

/// What a receipt body may contain; `deny_unknown_fields` lets a stored one be revalidated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum OperationResult {
    Script {
        exit_code: i32,
        timed_out: bool,
        stdout: String,
        stderr: String,
    },
    Read {
        path: String,
        size_bytes: u64,
        truncated: bool,
        content: String,
    },
    Write {
        path: String,
        bytes_written: usize,
    },
    Delete {
        deleted: String,
    },
    Rename {
        path: String,
        to_path: String,
    },
    List {
        path: String,
        entries: Vec<ListEntry>,
    },
    Glob {
        matches: Vec<String>,
    },
    Grep {
        matches: Vec<GrepMatch>,
    },
    Failure {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListEntry {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepMatch {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}
