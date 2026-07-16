//! model-bench.jsonl appender

#![allow(dead_code)]

use std::fmt;
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

/// Structured adversarial-review metadata layered onto the shared model-bench row.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdversarialLedgerRow {
    #[serde(flatten)]
    pub(crate) base: LedgerRow,
    pub(crate) review_id: String,
    pub(crate) provider: String,
    pub(crate) attempt_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reviewer_id: Option<String>,
    pub(crate) schema_valid: bool,
}

/// Appends one JSON row and trailing newline to `path`, creating parent dirs.
pub(crate) fn append(path: &Path, row: &LedgerRow) -> Result<()> {
    append_serialized(path, row)
}

/// Appends one adversarial attempt row with structured review metadata.
pub(crate) fn append_adversarial(path: &Path, row: &AdversarialLedgerRow) -> Result<()> {
    append_serialized(path, row)
}

fn append_serialized(path: &Path, row: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            LedgerError::new(format!(
                "failed to create ledger dir {}: {e}",
                parent.display()
            ))
        })?;
    }

    // Serialize the new row (with its trailing newline) into a buffer up
    // front. Reading any existing contents lets us replace the whole file
    // in one shot, so the temp + rename below is a true atomic replacement
    // — not just an append to the original. Mirrors `ratchet.rs::save`:
    // an interrupted write cannot leave the ledger in a partially-written
    // state because the original file is untouched until the rename lands.
    let mut new_row = serde_json::to_vec(row)
        .map_err(|e| LedgerError::new(format!("failed to serialize ledger row: {e}")))?;
    new_row.push(b'\n');

    let existing = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(LedgerError::new(format!(
                "failed to read ledger {}: {e}",
                path.display()
            )));
        }
    };

    let mut contents = existing;
    contents.extend_from_slice(&new_row);

    // Write the full intended contents to a sibling temp file in the same
    // directory, then rename it over the original. A sibling (not /tmp)
    // keeps the rename atomic on the same filesystem.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &contents).map_err(|e| {
        LedgerError::new(format!(
            "failed to write ledger temp {}: {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LedgerError::new(format!(
            "failed to rename ledger temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
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
            notes:
                "conductor arena arena-20260704-225738-warden-vy1 profile=pi-nw-kimi-k26 reason="
                    .to_string(),
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
        assert_eq!(
            parsed["arena_run_id"],
            json!("arena-20260704-225738-warden-vy1")
        );
        assert_eq!(parsed["winner"], json!(true));
        assert_eq!(parsed["applied"], json!(true));
        assert_eq!(parsed["duration_ms"], json!(120_000));
        assert_eq!(parsed["ralph_duration_ms"], json!(90_000));
        assert_eq!(parsed["verify_duration_ms"], json!(30_000));
        assert_eq!(parsed["tokens_used"], json!(309_466));
        assert_eq!(parsed["reasoning_effort"], json!("high"));
        assert!(parsed.get("cost_usd").is_none());
    }

    #[test]
    fn adversarial_append_serializes_structured_attempt_metadata() {
        let temp = TempDir::new("ledger-adversarial");
        let path = temp.path().join("model-bench.jsonl");
        let row = AdversarialLedgerRow {
            base: LedgerRow {
                date: "2026-07-15".to_string(),
                model: "openai-codex/gpt-5.6-luna".to_string(),
                harness: Some("pi".to_string()),
                profile: Some("luna-reviewer".to_string()),
                reasoning_effort: Some("high".to_string()),
                role: "adversarial-reviewer".to_string(),
                task: "review-123".to_string(),
                score_1_5: None,
                blind_rank: None,
                judge: None,
                verify_passed: false,
                complexity: "L".to_string(),
                project: "conductor".to_string(),
                bias_note: None,
                notes: "reviewer schema failure".to_string(),
                arena_run_id: None,
                winner: None,
                applied: None,
                failure_reason: Some("invalid JSON".to_string()),
                duration_ms: Some(17),
                ralph_duration_ms: None,
                verify_duration_ms: None,
                tokens_used: None,
                cost_usd: None,
            },
            review_id: "review-123".to_string(),
            provider: "openai".to_string(),
            attempt_kind: "repair".to_string(),
            reviewer_id: Some("R1".to_string()),
            schema_valid: false,
        };

        append_adversarial(&path, &row).expect("append adversarial row");

        let parsed: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(parsed["role"], json!("adversarial-reviewer"));
        assert_eq!(parsed["review_id"], json!("review-123"));
        assert_eq!(parsed["provider"], json!("openai"));
        assert_eq!(parsed["attempt_kind"], json!("repair"));
        assert_eq!(parsed["reviewer_id"], json!("R1"));
        assert_eq!(parsed["schema_valid"], json!(false));
        assert_eq!(parsed["failure_reason"], json!("invalid JSON"));
    }

    fn minimal_row(task: &str) -> LedgerRow {
        LedgerRow {
            date: "2026-07-16".to_string(),
            model: "fake-worker".to_string(),
            harness: None,
            profile: None,
            reasoning_effort: None,
            role: "implement".to_string(),
            task: task.to_string(),
            score_1_5: None,
            blind_rank: None,
            judge: None,
            verify_passed: true,
            complexity: "S".to_string(),
            project: "sandbox-repo".to_string(),
            bias_note: None,
            notes: String::new(),
            arena_run_id: None,
            winner: None,
            applied: None,
            failure_reason: None,
            duration_ms: None,
            ralph_duration_ms: None,
            verify_duration_ms: None,
            tokens_used: None,
            cost_usd: None,
        }
    }

    #[test]
    fn append_is_atomic_and_preserves_existing_rows() {
        // Two appends must both survive the read-modify-write + rename,
        // the ledger must hold two complete JSON lines (no partial row),
        // and the sibling temp file must not be left behind after a
        // successful append.
        let temp = TempDir::new("ledger-atomic");
        let path = temp.path().join("model-bench.jsonl");

        append(&path, &minimal_row("sandbox-1")).expect("first append");
        append(&path, &minimal_row("sandbox-2")).expect("second append");

        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file must be renamed away after a successful append"
        );

        let content = std::fs::read_to_string(&path).expect("read ledger");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "both rows must survive the append");
        for line in &lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each line is a complete JSON row");
            assert!(parsed.get("task").is_some());
        }
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["task"], json!("sandbox-1"));
        assert_eq!(second["task"], json!("sandbox-2"));
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
