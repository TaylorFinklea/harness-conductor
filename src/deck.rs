//! report.json writer (atomic), responses.json reader, live patcher

// Built ahead of the M3 dry-run integration path; unit tests exercise this module directly.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const SCHEMA: &str = "harness-deck/report@1";
const PROJECT: &str = "conductor";
const HARNESS: &str = "conductor";
const MAX_RUN_ID_LEN: usize = 200;

pub(crate) type Result<T> = std::result::Result<T, DeckError>;

/// Error returned by harness-deck report IO, response parsing, or validation.
#[derive(Debug, Clone)]
pub(crate) struct DeckError {
    message: String,
}

impl DeckError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(action: &str, path: &Path, source: &std::io::Error) -> Self {
        Self::new(format!("{action} {}: {source}", path.display()))
    }

    fn json(action: &str, source: &serde_json::Error) -> Self {
        Self::new(format!("{action}: {source}"))
    }

    fn command(command: &str, status: Option<i32>, stdout: &str, stderr: &str) -> Self {
        let status = status.map_or_else(|| "signal".to_string(), |code| code.to_string());
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Self::new(format!(
            "command `{command}` failed with status {status}: {detail}"
        ))
    }
}

impl fmt::Display for DeckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeckError {}

/// Harness-deck report status lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ReportStatus {
    /// Report is being assembled and is not ready for review.
    Draft,
    /// Report is ready and waiting for a human response.
    AwaitingReview,
    /// At least one interactive response has been recorded.
    Answered,
    /// Report is complete and no longer expects interaction.
    Done,
}

/// A conductor-owned harness-deck report manifest.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    schema: &'static str,
    id: String,
    project: &'static str,
    harness: &'static str,
    title: String,
    status: ReportStatus,
    created: String,
    blocks: Vec<Block>,
}

impl Report {
    /// Creates a conductor report manifest with the fixed harness-deck schema.
    pub(crate) fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        created: impl Into<String>,
        status: ReportStatus,
        blocks: Vec<Block>,
    ) -> Result<Self> {
        let id = id.into();
        validate_run_id(&id)?;
        Ok(Self {
            schema: SCHEMA,
            id,
            project: PROJECT,
            harness: HARNESS,
            title: title.into(),
            status,
            created: created.into(),
            blocks,
        })
    }

    /// Creates a terminal report that no longer expects interactive responses.
    pub(crate) fn completed(
        id: impl Into<String>,
        title: impl Into<String>,
        created: impl Into<String>,
        blocks: Vec<Block>,
    ) -> Result<Self> {
        Self::new(id, title, created, ReportStatus::Done, blocks)
    }

    /// Returns the run id used as the harness-deck run directory name.
    #[must_use]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

/// Supported conductor report block shapes for v1 cycle reports.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Block {
    /// Metric grid with optional progress bars.
    Metrics {
        title: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        metrics: Vec<Metric>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        bars: Vec<Bar>,
    },
    /// Simple columnar table.
    Table {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// Harness-deck approval prompt block.
    Approval { id: String, prompt: String },
    /// Highlighted aside for escalations and warnings.
    Callout {
        level: CalloutLevel,
        tag: String,
        markdown: String,
    },
}

impl Block {
    /// Creates a metrics block.
    #[must_use]
    pub(crate) fn metrics(title: impl Into<String>, metrics: Vec<Metric>, bars: Vec<Bar>) -> Self {
        Self::Metrics {
            title: title.into(),
            metrics,
            bars,
        }
    }

    /// Creates a table block from string-like columns and cells.
    #[must_use]
    pub(crate) fn table<C, R>(title: impl Into<String>, columns: C, rows: R) -> Self
    where
        C: IntoIterator,
        C::Item: Into<String>,
        R: IntoIterator,
        R::Item: IntoIterator,
        <R::Item as IntoIterator>::Item: Into<String>,
    {
        Self::Table {
            title: title.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(Into::into).collect())
                .collect(),
        }
    }

    /// Creates an approval block.
    #[must_use]
    pub(crate) fn approval(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self::Approval {
            id: id.into(),
            prompt: prompt.into(),
        }
    }

    /// Creates a callout block.
    #[must_use]
    pub(crate) fn callout(
        level: CalloutLevel,
        tag: impl Into<String>,
        markdown: impl Into<String>,
    ) -> Self {
        Self::Callout {
            level,
            tag: tag.into(),
            markdown: markdown.into(),
        }
    }
}

/// One metric tile inside a metrics block.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Metric {
    label: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trend: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    spark: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

impl Metric {
    /// Creates a metric tile with a label and display value.
    #[must_use]
    pub(crate) fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            unit: None,
            delta: None,
            trend: None,
            spark: Vec::new(),
            color: None,
        }
    }

    /// Sets the metric unit string.
    #[must_use]
    pub(crate) fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Sets the metric delta string.
    #[must_use]
    pub(crate) fn with_delta(mut self, delta: impl Into<String>) -> Self {
        self.delta = Some(delta.into());
        self
    }

    /// Sets the metric trend string (`pos`, `neg`, or renderer-supported values).
    #[must_use]
    pub(crate) fn with_trend(mut self, trend: impl Into<String>) -> Self {
        self.trend = Some(trend.into());
        self
    }

    /// Sets the sparkline samples.
    #[must_use]
    pub(crate) fn with_spark(mut self, spark: Vec<u64>) -> Self {
        self.spark = spark;
        self
    }

    /// Sets the renderer color hint.
    #[must_use]
    pub(crate) fn with_color(mut self, color: impl Into<String>) -> Self {
        self.color = Some(color.into());
        self
    }
}

/// One progress bar inside a metrics block.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Bar {
    label: String,
    pct: u8,
    color: String,
}

impl Bar {
    /// Creates a labeled percent bar.
    #[must_use]
    pub(crate) fn new(label: impl Into<String>, pct: u8, color: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            pct,
            color: color.into(),
        }
    }
}

/// Severity level for a callout block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CalloutLevel {
    /// Informational callout.
    Info,
    /// Warning callout.
    Warn,
    /// Error callout.
    Err,
}

/// Live/in-flight telemetry patch for a report manifest.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveUpdate {
    updated: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f64>,
}

impl LiveUpdate {
    /// Creates a live patch with the required `updated` timestamp.
    #[must_use]
    pub(crate) fn new(updated: impl Into<String>) -> Self {
        Self {
            updated: updated.into(),
            step: None,
            elapsed_ms: None,
            tokens: None,
            cost_usd: None,
            progress: None,
        }
    }

    /// Sets the current step label.
    #[must_use]
    pub(crate) fn with_step(mut self, step: impl Into<String>) -> Self {
        self.step = Some(step.into());
        self
    }

    /// Sets elapsed milliseconds since cycle start.
    #[must_use]
    pub(crate) const fn with_elapsed_ms(mut self, elapsed_ms: u64) -> Self {
        self.elapsed_ms = Some(elapsed_ms);
        self
    }

    /// Sets cumulative token count.
    #[must_use]
    pub(crate) const fn with_tokens(mut self, tokens: u64) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Sets cumulative cost in USD as a precision-preserving string.
    #[must_use]
    pub(crate) fn with_cost_usd(mut self, cost_usd: impl Into<String>) -> Self {
        self.cost_usd = Some(cost_usd.into());
        self
    }

    /// Sets progress as a 0..1 fraction.
    #[must_use]
    pub(crate) const fn with_progress(mut self, progress: f64) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// Parsed harness-deck responses for one run directory.
#[derive(Debug, Clone)]
pub(crate) struct Responses {
    version: u64,
    responses: BTreeMap<String, Response>,
}

impl Responses {
    /// Returns the effective responses schema version; missing/0 is normalized to 1.
    #[must_use]
    pub(crate) const fn version(&self) -> u64 {
        self.version
    }

    /// Returns a block response only when it is newer than the optional `at` watermark.
    #[must_use]
    pub(crate) fn response_after(
        &self,
        block_id: &str,
        watermark: Option<&str>,
    ) -> Option<&Response> {
        let response = self.responses.get(block_id)?;
        if watermark.is_some_and(|mark| response.at.as_str() <= mark) {
            None
        } else {
            Some(response)
        }
    }
}

/// Current answer for a harness-deck interactive block.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct Response {
    #[serde(default)]
    block: Option<String>,
    value: String,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    note: String,
    at: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Response {
    /// Returns the current response value.
    #[must_use]
    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    /// Returns any optional note attached to the response.
    #[must_use]
    pub(crate) fn note(&self) -> &str {
        &self.note
    }

    /// Returns the answer timestamp used as the polling watermark.
    #[must_use]
    pub(crate) fn at(&self) -> &str {
        &self.at
    }

    /// Returns multi-select values when the source block records them.
    #[must_use]
    pub(crate) fn values(&self) -> &[String] {
        &self.values
    }
}

#[derive(Debug, Deserialize)]
struct RawResponses {
    #[serde(default)]
    version: u64,
    #[serde(default)]
    responses: BTreeMap<String, Response>,
}

/// Reads `responses.json` from a harness-deck run directory.
pub(crate) fn read_responses(run_dir: &Path) -> Result<Responses> {
    let path = run_dir.join("responses.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Responses {
                version: 1,
                responses: BTreeMap::new(),
            });
        }
        Err(e) => return Err(DeckError::io("failed to read", &path, &e)),
    };
    let raw: RawResponses = serde_json::from_slice(&bytes)
        .map_err(|e| DeckError::json("failed to parse responses", &e))?;
    Ok(Responses {
        version: normalize_response_version(raw.version),
        responses: raw.responses,
    })
}

/// Returns `~/.harness/reports/conductor/<run-id>` under `home`.
pub(crate) fn report_run_dir(home: &Path, run_id: &str) -> Result<PathBuf> {
    validate_run_id(run_id)?;
    Ok(home
        .join(".harness")
        .join("reports")
        .join(PROJECT)
        .join(run_id))
}

/// Returns `~/.harness/reports/conductor/<run-id>/report.json` under `home`.
pub(crate) fn report_path(home: &Path, run_id: &str) -> Result<PathBuf> {
    Ok(report_run_dir(home, run_id)?.join("report.json"))
}

/// Writes a conductor report to `~/.harness/reports/conductor/<run-id>/report.json` under `home`.
pub(crate) fn write_report(home: &Path, report: &Report) -> Result<PathBuf> {
    let run_dir = report_run_dir(home, report.id())?;
    fs::create_dir_all(&run_dir).map_err(|e| DeckError::io("failed to create", &run_dir, &e))?;
    let report_path = run_dir.join("report.json");
    let mut bytes = serde_json::to_vec_pretty(report)
        .map_err(|e| DeckError::json("failed to serialize report", &e))?;
    bytes.push(b'\n');
    atomic_write_bytes(&report_path, &bytes)?;
    Ok(report_path)
}

/// Patches the manifest status, preserving unmodeled JSON fields elsewhere.
pub(crate) fn patch_status(report_path: &Path, status: ReportStatus) -> Result<()> {
    let bytes =
        fs::read(report_path).map_err(|e| DeckError::io("failed to read", report_path, &e))?;
    let mut manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| DeckError::json("failed to parse report", &e))?;
    let Some(root) = manifest.as_object_mut() else {
        return Err(DeckError::new("report manifest must be a JSON object"));
    };
    root.insert(
        "status".to_string(),
        serde_json::to_value(status)
            .map_err(|e| DeckError::json("failed to serialize status", &e))?,
    );
    let mut output = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| DeckError::json("failed to serialize patched report", &e))?;
    output.push(b'\n');
    atomic_write_bytes(report_path, &output)
}

/// Appends a callout block to an existing manifest, preserving unmodeled JSON fields elsewhere.
pub(crate) fn append_callout(
    report_path: &Path,
    level: CalloutLevel,
    tag: &str,
    markdown: &str,
) -> Result<()> {
    let bytes =
        fs::read(report_path).map_err(|e| DeckError::io("failed to read", report_path, &e))?;
    let mut manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| DeckError::json("failed to parse report", &e))?;
    let Some(root) = manifest.as_object_mut() else {
        return Err(DeckError::new("report manifest must be a JSON object"));
    };
    let blocks = root
        .entry("blocks".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !blocks.is_array() {
        *blocks = Value::Array(Vec::new());
    }
    let Some(blocks) = blocks.as_array_mut() else {
        return Err(DeckError::new("internal: blocks was not an array"));
    };
    let block = serde_json::to_value(Block::callout(level, tag, markdown))
        .map_err(|e| DeckError::json("failed to serialize callout", &e))?;
    blocks.push(block);

    let mut output = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| DeckError::json("failed to serialize patched report", &e))?;
    output.push(b'\n');
    atomic_write_bytes(report_path, &output)
}

/// Patches only the manifest's `live` object, preserving unmodeled JSON fields elsewhere.
pub(crate) fn patch_live(report_path: &Path, live: &LiveUpdate) -> Result<()> {
    let bytes =
        fs::read(report_path).map_err(|e| DeckError::io("failed to read", report_path, &e))?;
    let mut manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| DeckError::json("failed to parse report", &e))?;
    let live_patch = serde_json::to_value(live)
        .map_err(|e| DeckError::json("failed to serialize live patch", &e))?;
    let Some(root) = manifest.as_object_mut() else {
        return Err(DeckError::new("report manifest must be a JSON object"));
    };
    let live_entry = root
        .entry("live".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !live_entry.is_object() {
        *live_entry = Value::Object(Map::new());
    }
    let Some(live_object) = live_entry.as_object_mut() else {
        return Err(DeckError::new("internal: live object was not an object"));
    };
    let Some(patch_object) = live_patch.as_object() else {
        return Err(DeckError::new("internal: live patch was not an object"));
    };
    for (key, value) in patch_object {
        live_object.insert(key.clone(), value.clone());
    }

    let mut output = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| DeckError::json("failed to serialize patched report", &e))?;
    output.push(b'\n');
    atomic_write_bytes(report_path, &output)
}

/// Validator abstraction for `harness-deck validate` subprocesses.
pub(crate) trait DeckValidator {
    /// Validates one report manifest path.
    fn validate(&self, report_path: &Path) -> Result<()>;
}

/// `harness-deck validate` subprocess implementation.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandDeckValidator;

impl CommandDeckValidator {
    /// Creates a command-backed harness-deck validator.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl DeckValidator for CommandDeckValidator {
    fn validate(&self, report_path: &Path) -> Result<()> {
        let command = format!("harness-deck validate {}", report_path.display());
        let output = Command::new("harness-deck")
            .arg("validate")
            .arg(report_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| DeckError::new(format!("failed to spawn `{command}`: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(DeckError::command(
            &command,
            output.status.code(),
            &stdout,
            &stderr,
        ))
    }
}

fn normalize_response_version(version: u64) -> u64 {
    if version == 0 { 1 } else { version }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    let valid = !run_id.is_empty()
        && run_id.len() <= MAX_RUN_ID_LEN
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(DeckError::new(format!(
            "invalid run id {run_id:?}; expected ^[a-zA-Z0-9._-]+$ and at most {MAX_RUN_ID_LEN} bytes"
        )))
    }
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_bytes_with_hook(path, bytes, |_| Ok(()))
}

fn atomic_write_bytes_with_hook<F>(path: &Path, bytes: &[u8], before_rename: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| DeckError::new(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(parent).map_err(|e| DeckError::io("failed to create", parent, &e))?;
    let (tmp_path, mut file) = create_temp_file(parent, path)?;
    if let Err(err) = file.write_all(bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(DeckError::io("failed to write", &tmp_path, &err));
    }
    if let Err(err) = file.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(DeckError::io("failed to sync", &tmp_path, &err));
    }
    drop(file);

    if let Err(err) = before_rename(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(DeckError::io("failed to rename", path, &err));
    }
    Ok(())
}

fn create_temp_file(parent: &Path, destination: &Path) -> Result<(PathBuf, File)> {
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report");
    let nonce = temp_nonce();
    for attempt in 0_u8..100 {
        let tmp_path = parent.join(format!(".{base}.{nonce}.{attempt}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(DeckError::io("failed to create", &tmp_path, &e)),
        }
    }
    Err(DeckError::new(format!(
        "failed to create a unique temp file for {}",
        destination.display()
    )))
}

fn temp_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn write_report_rejects_invalid_run_id() {
        let err = Report::new(
            "bad/run",
            "bad run",
            "2026-07-02T00:00:00Z",
            ReportStatus::AwaitingReview,
            Vec::new(),
        )
        .expect_err("slash is outside the run-id charset");

        assert!(
            err.to_string().contains("invalid run id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn atomic_write_keeps_existing_report_until_complete_file_is_renamed() {
        let temp = TempDir::new("deck-atomic");
        let report_path = temp.path().join("report.json");
        std::fs::write(&report_path, br#"{"old":true}"#).expect("seed old report");

        atomic_write_bytes_with_hook(&report_path, br#"{"new":true}"#, |tmp_path| {
            let visible = std::fs::read_to_string(&report_path).expect("read visible report");
            let staged = std::fs::read_to_string(tmp_path).expect("read temp report");

            assert_eq!(visible, r#"{"old":true}"#);
            assert_eq!(staged, r#"{"new":true}"#);
            assert_ne!(tmp_path, report_path.as_path());
            Ok(())
        })
        .expect("atomic write succeeds");

        let final_report = std::fs::read_to_string(&report_path).expect("read final report");
        assert_eq!(final_report, r#"{"new":true}"#);
    }

    #[test]
    fn read_responses_absent_file_is_unanswered() {
        let temp = TempDir::new("deck-absent-responses");

        let responses = read_responses(temp.path()).expect("absent responses is not an error");

        assert_eq!(responses.version(), 1);
        assert!(responses.response_after("dispatch-plan", None).is_none());
    }

    #[test]
    fn read_responses_treats_versionless_file_as_version_1_and_applies_at_watermark() {
        let temp = TempDir::new("deck-versionless-responses");
        std::fs::write(
            temp.path().join("responses.json"),
            serde_json::to_vec_pretty(&json!({
                "run": "cycle-20260702-000000",
                "project": "conductor",
                "updated": "2026-07-02T00:02:00Z",
                "responses": {
                    "dispatch-plan": {
                        "block": "dispatch-plan",
                        "value": "approved",
                        "note": "ship it",
                        "at": "2026-07-02T00:01:00Z"
                    }
                }
            }))
            .expect("serialize responses"),
        )
        .expect("write responses");

        let responses = read_responses(temp.path()).expect("versionless responses parse");

        assert_eq!(responses.version(), 1);
        assert!(
            responses
                .response_after("dispatch-plan", Some("2026-07-02T00:01:00Z"))
                .is_none(),
            "answer at the watermark should not be returned again"
        );
        let response = responses
            .response_after("dispatch-plan", Some("2026-07-02T00:00:59Z"))
            .expect("newer answer returned");
        assert_eq!(response.value(), "approved");
        assert_eq!(response.note(), "ship it");
    }

    #[test]
    fn patch_live_updates_only_live_object_and_preserves_unknown_keys() {
        let temp = TempDir::new("deck-live-patch");
        let report_path = temp.path().join("report.json");
        let original = json!({
            "schema": "harness-deck/report@1",
            "id": "cycle-20260702-000000",
            "project": "conductor",
            "harness": "conductor",
            "title": "Cycle",
            "status": "awaiting-review",
            "created": "2026-07-02T00:00:00Z",
            "mystery": {"keep": true},
            "live": {"updated": "2026-07-02T00:00:00Z", "step": "old", "custom": 7},
            "blocks": [
                {"type": "approval", "id": "dispatch-plan", "prompt": "Run it?", "unknown": "preserve"}
            ]
        });
        std::fs::write(
            &report_path,
            serde_json::to_vec_pretty(&original).expect("serialize original"),
        )
        .expect("write original report");

        let live = LiveUpdate::new("2026-07-02T00:00:10Z")
            .with_step("dispatching")
            .with_elapsed_ms(10_000)
            .with_progress(0.5);
        patch_live(&report_path, &live).expect("patch live");

        let after: Value =
            serde_json::from_slice(&std::fs::read(&report_path).expect("read patched report"))
                .expect("parse patched report");
        assert_eq!(after["mystery"], original["mystery"]);
        assert_eq!(after["blocks"], original["blocks"]);
        assert_eq!(after["live"]["custom"], json!(7));
        assert_eq!(after["live"]["updated"], json!("2026-07-02T00:00:10Z"));
        assert_eq!(after["live"]["step"], json!("dispatching"));
        assert_eq!(after["live"]["elapsed_ms"], json!(10_000));
        assert_eq!(after["live"]["progress"], json!(0.5));
    }

    #[test]
    fn generated_sample_report_passes_harness_deck_validate() {
        let temp = TempDir::new("deck-validate");
        let report = sample_report().expect("sample report is valid");

        let report_path = write_report(temp.path(), &report).expect("write report");
        CommandDeckValidator::new()
            .validate(&report_path)
            .expect("harness-deck validates generated report");

        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&report_path).expect("read generated report"))
                .expect("parse generated report");
        assert_eq!(manifest["schema"], json!("harness-deck/report@1"));
        assert_eq!(manifest["project"], json!("conductor"));
        assert_eq!(manifest["harness"], json!("conductor"));
        assert_eq!(manifest["status"], json!("awaiting-review"));
    }

    #[test]
    fn completed_report_serializes_terminal_done_status() {
        let temp = TempDir::new("deck-completed");
        let report = Report::completed(
            "adversarial-review-1",
            "Adversarial design review",
            "2026-07-15T00:00:00Z",
            vec![Block::callout(
                CalloutLevel::Info,
                "OUTCOME",
                "Complete synthesis",
            )],
        )
        .expect("completed report");

        let path = write_report(temp.path(), &report).expect("write completed report");
        let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(manifest["status"], json!("done"));
        assert_eq!(manifest["blocks"][0]["tag"], json!("OUTCOME"));
    }

    fn sample_report() -> Result<Report> {
        Report::new(
            "cycle-20260702-000000",
            "Conductor dry-run",
            "2026-07-02T00:00:00Z",
            ReportStatus::AwaitingReview,
            vec![
                Block::metrics(
                    "cycle metrics",
                    vec![
                        Metric::new("repos scanned", "24"),
                        Metric::new("ready items", "231"),
                        Metric::new("triaged", "72").with_unit("%"),
                    ],
                    vec![
                        Bar::new("triaged", 72, "cyan"),
                        Bar::new("flagged", 18, "yellow"),
                    ],
                ),
                Block::table(
                    "queue",
                    vec!["repo", "ready", "state"],
                    vec![
                        vec!["conductor", "6", "ready"],
                        vec!["tesela", "0", "blocked"],
                    ],
                ),
                Block::approval("dispatch-plan", "Approve the proposed dispatch plan?"),
                Block::callout(
                    CalloutLevel::Warn,
                    "ESC",
                    "Two items are missing `verify_cmd` and need triage.",
                ),
            ],
        )
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-tmp")
                .join(format!("conductor-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp dir");
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
