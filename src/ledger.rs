//! model-bench.jsonl appender

#![allow(dead_code)]

use std::fmt;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

pub(crate) type Result<T> = std::result::Result<T, LedgerError>;

#[derive(Debug, Clone)]
pub(crate) struct LedgerError {
    message: String,
}

impl LedgerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LedgerError {}

/// One Conductor dispatch row for `~/.claude/model-bench.jsonl`.
///
/// Mirrors the current model-bench JSONL rows (`date`, `model`, `role`,
/// optional task label, `verify_passed`, `complexity`, `project`, `notes`) and
/// keeps score/harness fields optional so ordinary dispatch rows stay light
/// while Arena rows can record head-to-head results.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LedgerRow {
    pub(crate) date: String,
    pub(crate) model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile: Option<String>,
    pub(crate) role: String,
    pub(crate) task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) score_1_5: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blind_rank: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) judge: Option<String>,
    pub(crate) verify_passed: bool,
    pub(crate) complexity: String,
    pub(crate) project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bias_note: Option<String>,
    pub(crate) notes: String,
}

/// Appends one JSON row and trailing newline to `path`, creating parent dirs.
pub(crate) fn append(path: &Path, row: &LedgerRow) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            LedgerError::new(format!(
                "failed to create ledger dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| LedgerError::new(format!("failed to open ledger {}: {e}", path.display())))?;
    serde_json::to_writer(&mut file, row)
        .map_err(|e| LedgerError::new(format!("failed to serialize ledger row: {e}")))?;
    file.write_all(b"\n")
        .map_err(|e| LedgerError::new(format!("failed to write ledger {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn append_writes_one_row_without_score() {
        let temp = TempDir::new("ledger");
        let path = temp.path().join("model-bench.jsonl");
        let row = LedgerRow {
            date: "2026-07-02".to_string(),
            model: "fake-worker".to_string(),
            harness: None,
            profile: None,
            role: "implement".to_string(),
            task: "sandbox-1".to_string(),
            score_1_5: None,
            blind_rank: None,
            judge: None,
            verify_passed: true,
            complexity: "S".to_string(),
            project: "sandbox-repo".to_string(),
            bias_note: None,
            notes: "conductor cycle-1: verified".to_string(),
        };

        append(&path, &row).expect("append ledger");

        let content = std::fs::read_to_string(&path).expect("read ledger");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).expect("json row");
        assert_eq!(parsed["date"], json!("2026-07-02"));
        assert_eq!(parsed["model"], json!("fake-worker"));
        assert_eq!(parsed["role"], json!("implement"));
        assert_eq!(parsed["task"], json!("sandbox-1"));
        assert_eq!(parsed["verify_passed"], json!(true));
        assert_eq!(parsed["complexity"], json!("S"));
        assert_eq!(parsed["project"], json!("sandbox-repo"));
        assert_eq!(parsed["notes"], json!("conductor cycle-1: verified"));
        assert!(parsed.get("score_1_5").is_none());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
