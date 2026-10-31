//! The receipt journal: every step of one operation is on disk before its network call.
//!
//! States per operation: `received` → `started` → `terminal` (result stored, fsynced) →
//! `acknowledged`. On restart a `terminal` without `acknowledged` posts its stored result again
//! under the same fence, and a `started` without `terminal` is never re-run: it may already have
//! executed, so the server must resolve it as `unknown`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::models::OperationResult;
use crate::output::CliError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Record {
    Received {
        operation_id: String,
        seq: i64,
    },
    Started {
        operation_id: String,
    },
    Terminal {
        operation_id: String,
        receipt_status: String,
        result: OperationResult,
    },
    Acknowledged {
        operation_id: String,
    },
}

impl Record {
    fn operation_id(&self) -> &str {
        match self {
            Self::Received { operation_id, .. }
            | Self::Started { operation_id }
            | Self::Terminal { operation_id, .. }
            | Self::Acknowledged { operation_id } => operation_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JournalReceipt {
    pub(crate) operation_id: String,
    pub(crate) receipt_status: String,
    pub(crate) result: OperationResult,
}

#[derive(Debug)]
pub(crate) struct Journal {
    path: PathBuf,
}

impl Journal {
    pub(crate) fn open(root: &Path, assignment_id: &str) -> Result<Self, CliError> {
        fs::create_dir_all(root).map_err(|e| CliError::io("could not create the journal", &e))?;
        Ok(Self {
            path: root.join(format!("{assignment_id}.jsonl")),
        })
    }

    fn append(&self, record: &Record) -> Result<(), CliError> {
        let fail = |e: std::io::Error| CliError::io("could not write the journal", &e);
        let mut line = serde_json::to_vec(record)
            .map_err(|e| CliError::new("internal", format!("journal encode: {e}")))?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600) // results can hold what a command printed
            .open(&self.path)
            .map_err(fail)?;
        file.write_all(&line)
            .and_then(|()| file.sync_all())
            .map_err(fail)
    }

    pub(crate) fn received(&self, operation_id: &str, seq: i64) -> Result<(), CliError> {
        self.append(&Record::Received {
            operation_id: operation_id.to_owned(),
            seq,
        })
    }

    pub(crate) fn started(&self, operation_id: &str) -> Result<(), CliError> {
        self.append(&Record::Started {
            operation_id: operation_id.to_owned(),
        })
    }

    pub(crate) fn terminal(
        &self,
        operation_id: &str,
        status: &str,
        result: &OperationResult,
    ) -> Result<(), CliError> {
        self.append(&Record::Terminal {
            operation_id: operation_id.to_owned(),
            receipt_status: status.to_owned(),
            result: result.clone(),
        })
    }

    pub(crate) fn acknowledged(&self, operation_id: &str) -> Result<(), CliError> {
        self.append(&Record::Acknowledged {
            operation_id: operation_id.to_owned(),
        })
    }

    /// A torn trailing line from a hard kill is skipped: everything before it stands.
    fn latest(&self) -> Result<BTreeMap<String, Record>, CliError> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(CliError::io("could not read the journal", &e)),
        };
        let mut latest = BTreeMap::new();
        for line in raw.split(|byte| *byte == b'\n') {
            if let Ok(record) = serde_json::from_slice::<Record>(line) {
                latest.insert(record.operation_id().to_owned(), record);
            }
        }
        Ok(latest)
    }

    /// Terminal records the server may not have confirmed.
    pub(crate) fn unacknowledged_results(&self) -> Result<Vec<JournalReceipt>, CliError> {
        Ok(self
            .latest()?
            .into_values()
            .filter_map(|record| match record {
                Record::Terminal {
                    operation_id,
                    receipt_status,
                    result,
                } => Some(JournalReceipt {
                    operation_id,
                    receipt_status,
                    result,
                }),
                _ => None,
            })
            .collect())
    }

    /// Started without a terminal record: these operations must never run again.
    pub(crate) fn interrupted(&self) -> Result<Vec<String>, CliError> {
        Ok(self
            .latest()?
            .into_iter()
            .filter(|(_, record)| matches!(record, Record::Started { .. }))
            .map(|(id, _)| id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::Journal;
    use crate::models::OperationResult;

    #[test]
    fn records_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "a").unwrap();
        let result = OperationResult::Delete {
            deleted: "x".into(),
        };
        journal.received("done", 1).unwrap();
        journal.started("done").unwrap();
        journal.terminal("done", "succeeded", &result).unwrap();
        journal.started("lost").unwrap();
        journal.started("acked").unwrap();
        journal.terminal("acked", "succeeded", &result).unwrap();
        journal.acknowledged("acked").unwrap();
        let reopened = Journal::open(dir.path(), "a").unwrap();
        let pending = reopened.unacknowledged_results().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].operation_id.as_str(), &pending[0].result),
            ("done", &result)
        );
        assert_eq!(reopened.interrupted().unwrap(), ["lost"]);
    }

    #[test]
    fn tolerates_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "a").unwrap();
        journal.started("one").unwrap();
        let path = dir.path().join("a.jsonl");
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(br#"{"state":"termi"#);
        std::fs::write(&path, raw).unwrap();
        assert_eq!(journal.interrupted().unwrap(), ["one"]);
    }

    #[test]
    fn reads_a_journal_the_python_worker_wrote() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.jsonl"),
            concat!(
                r#"{"state": "received", "operation_id": "p", "seq": 1}"#, "\n",
                r#"{"state": "started", "operation_id": "p"}"#, "\n",
                r#"{"state": "terminal", "operation_id": "p", "receipt_status": "failed", "result": {"error": "boom"}}"#, "\n",
            ),
        )
        .unwrap();
        let journal = Journal::open(dir.path(), "a").unwrap();
        let pending = journal.unacknowledged_results().unwrap();
        assert_eq!(
            pending[0].result,
            OperationResult::Failure {
                error: "boom".into()
            }
        );
    }
}
