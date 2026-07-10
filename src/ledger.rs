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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) arena_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) winner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) applied: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ralph_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verify_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tokens_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost_usd: Option<String>,
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
            reasoning_effort: None,
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
            arena_run_id: None,
            winner: None,
            applied: None,
            failure_reason: None,
            duration_ms: None,
            ralph_duration_ms: None,
            verify_duration_ms: None,
            tokens_used: None,
            cost_usd: None,
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
        assert!(parsed.get("reasoning_effort").is_none());
    }

    #[test]
    fn append_writes_arena_metadata_when_present() {
        let temp = TempDir::new("ledger-arena");
        let path = temp.path().join("model-bench.jsonl");
        let row = LedgerRow {
            date: "2026-07-04".to_string(),
            model: "neuralwatt/kimi-k2.6".to_string(),
            harness: Some("pi".to_string()),
            profile: Some("pi-nw-kimi-k26".to_string()),
            reasoning_effort: Some("high".to_string()),
            role: "arena-candidate".to_string(),
            task: "warden-vy1".to_string(),
            score_1_5: Some(4.4),
            blind_rank: Some(1),
            judge: Some("qwen37max,gpt55,nw-glm52".to_string()),
            verify_passed: true,
            complexity: "S".to_string(),
            project: "warden".to_string(),
            bias_note: Some("arena blind panel".to_string()),
            notes: "conductor arena arena-20260704-225738-warden-vy1 profile=pi-nw-kimi-k26 reason=".to_string(),
            arena_run_id: Some("arena-20260704-225738-warden-vy1".to_string()),
            winner: Some(true),
            applied: Some(true),
            failure_reason: None,
            duration_ms: Some(120_000),
            ralph_duration_ms: Some(90_000),
            verify_duration_ms: Some(30_000),
            tokens_used: Some(309_466),
            cost_usd: None,
        };

        append(&path, &row).expect("append ledger");

        let content = std::fs::read_to_string(&path).expect("read ledger");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).expect("json row");
        assert_eq!(parsed["arena_run_id"], json!("arena-20260704-225738-warden-vy1"));
        assert_eq!(parsed["winner"], json!(true));
        assert_eq!(parsed["applied"], json!(true));
        assert_eq!(parsed["duration_ms"], json!(120_000));
        assert_eq!(parsed["ralph_duration_ms"], json!(90_000));
        assert_eq!(parsed["verify_duration_ms"], json!(30_000));
        assert_eq!(parsed["tokens_used"], json!(309_466));
        assert_eq!(parsed["reasoning_effort"], json!("high"));
        assert!(parsed.get("cost_usd").is_none());
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
