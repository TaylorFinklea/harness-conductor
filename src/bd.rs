//! bd subprocess client behind a trait (`BdClient`) so tests use fixtures.
//!
//! Fixture capture against throwaway bd repos confirmed both `bd ready --json`
//! zero states render identically as `[]`: a drained repo with no open issues
//! and a repo whose open work is entirely blocked. Callers must distinguish
//! those states with `bd count --json` (`{"count":0,"schema_version":1}` for
//! drained) and `bd blocked --json` (`[]` when no blocked issues, issue arrays
//! when blocked work exists).

// The client is built ahead of its milestone consumers (scan/status and later
// verify/dispatch). Unit tests exercise it directly until those modules call it.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

pub(crate) type Result<T> = std::result::Result<T, BdError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BdErrorKind {
    Other,
    Json,
    Command,
}

#[derive(Debug, Clone)]
pub(crate) struct BdError {
    message: String,
    kind: BdErrorKind,
}

impl BdError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: BdErrorKind::Other,
        }
    }

    pub(crate) fn json(command: &str, source: &serde_json::Error) -> Self {
        Self {
            message: format!("failed to parse JSON from `{command}`: {source}"),
            kind: BdErrorKind::Json,
        }
    }

    pub(crate) const fn is_json_parse(&self) -> bool {
        matches!(self.kind, BdErrorKind::Json)
    }

    fn command(command: &str, status: Option<i32>, stdout: &str, stderr: &str) -> Self {
        let status = status.map_or_else(|| "signal".to_string(), |code| code.to_string());
        let detail =
            parse_cli_error(stdout).map_or_else(|| stderr.trim().to_string(), |body| body.error);
        let detail = if detail.is_empty() {
            stdout.trim().to_string()
        } else {
            detail
        };
        Self {
            message: format!("bd command `{command}` failed with status {status}: {detail}"),
            kind: BdErrorKind::Command,
        }
    }
}

impl fmt::Display for BdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BdError {}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
pub(crate) struct Issue {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) acceptance_criteria: String,
    #[serde(default)]
    pub(crate) notes: String,
    pub(crate) status: String,
    pub(crate) priority: u32,
    #[expect(
        clippy::struct_field_names,
        reason = "bd JSON field is named issue_type"
    )]
    pub(crate) issue_type: String,
    #[serde(default)]
    pub(crate) assignee: Option<String>,
    pub(crate) owner: String,
    pub(crate) created_at: String,
    pub(crate) created_by: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) started_at: Option<String>,
    #[serde(default)]
    pub(crate) labels: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) estimated_minutes: Option<u32>,
    #[serde(default)]
    pub(crate) metadata: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    pub(crate) parent: Option<String>,
    // Real `bd ready --json` output emits an array of dependency-edge objects
    // (issue_id/depends_on_id/type/created_at/created_by/metadata), not plain
    // issue-id strings; a `Vec<String>` typing made the whole array — and thus
    // the whole `bd ready` parse — fail for any issue with populated deps,
    // silently emptying that repo's ready list (conductor-guildhall-dogfood
    // fix, 2026-07-02). This field is otherwise unused by triage/scan logic,
    // so `Value` avoids over-modeling a shape nothing consumes.
    #[serde(default)]
    pub(crate) dependencies: Option<Vec<Value>>,
    #[serde(default)]
    pub(crate) dependency_count: Option<u32>,
    #[serde(default)]
    pub(crate) dependent_count: Option<u32>,
    #[serde(default)]
    pub(crate) comment_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, serde::Serialize)]
pub(crate) struct Comment {
    pub(crate) id: String,
    pub(crate) issue_id: String,
    pub(crate) text: String,
    pub(crate) author: String,
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) schema_version: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct CliErrorBody {
    pub(crate) error: String,
    #[serde(default)]
    pub(crate) schema_version: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CountBody {
    count: u64,
}

pub(crate) trait BdClient {
    fn ready(&self, repo: &Path) -> Result<Vec<Issue>>;
    fn show(&self, repo: &Path, id: &str) -> Result<Issue>;
    fn count(&self, repo: &Path) -> Result<u64>;
    fn blocked(&self, repo: &Path) -> Result<Vec<Issue>>;
    fn claim(&self, repo: &Path, id: &str, actor: &str) -> Result<Issue>;
    fn release(&self, repo: &Path, id: &str) -> Result<Issue>;
    fn close(&self, repo: &Path, id: &str, reason: &str) -> Result<Issue>;
    fn comment(&self, repo: &Path, id: &str, text: &str) -> Result<Comment>;
    fn set_metadata(&self, repo: &Path, id: &str, key: &str, value: &str) -> Result<Issue>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandBdClient;

impl CommandBdClient {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self
    }

    fn run_json(repo: &Path, args: &[String]) -> Result<String> {
        let command = display_command(repo, args);
        let output = Command::new("bd")
            .arg("-C")
            .arg(repo)
            .args(args)
            .arg("--json")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| BdError::new(format!("failed to spawn `{command}`: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(BdError::command(
                &command,
                output.status.code(),
                &stdout,
                &stderr,
            ));
        }
        Ok(stdout)
    }
}

impl BdClient for CommandBdClient {
    fn ready(&self, repo: &Path) -> Result<Vec<Issue>> {
        let args = strings(["ready"]);
        let output = Self::run_json(repo, &args)?;
        parse_issue_array(&output, "bd ready")
    }

    fn show(&self, repo: &Path, id: &str) -> Result<Issue> {
        let args = strings(["show", id]);
        let output = Self::run_json(repo, &args)?;
        parse_single_issue(&output, "bd show")
    }

    fn count(&self, repo: &Path) -> Result<u64> {
        let args = strings(["count"]);
        let output = Self::run_json(repo, &args)?;
        parse_count(&output, "bd count")
    }

    fn blocked(&self, repo: &Path) -> Result<Vec<Issue>> {
        let args = strings(["blocked"]);
        let output = Self::run_json(repo, &args)?;
        parse_issue_array(&output, "bd blocked")
    }

    fn claim(&self, repo: &Path, id: &str, actor: &str) -> Result<Issue> {
        let args = strings(["--actor", actor, "update", id, "--claim"]);
        let output = Self::run_json(repo, &args)?;
        parse_single_issue(&output, "bd update --claim")
    }

    fn release(&self, repo: &Path, id: &str) -> Result<Issue> {
        let args = strings(["update", id, "--status", "open", "--assignee", ""]);
        let output = Self::run_json(repo, &args)?;
        parse_single_issue(&output, "bd update release")
    }

    fn close(&self, repo: &Path, id: &str, reason: &str) -> Result<Issue> {
        let args = strings(["close", id, "--reason", reason]);
        let output = Self::run_json(repo, &args)?;
        parse_single_issue(&output, "bd close")
    }

    fn comment(&self, repo: &Path, id: &str, text: &str) -> Result<Comment> {
        let args = strings(["comment", id, text]);
        let output = Self::run_json(repo, &args)?;
        parse_comment(&output, "bd comment")
    }

    fn set_metadata(&self, repo: &Path, id: &str, key: &str, value: &str) -> Result<Issue> {
        let pair = format!("{key}={value}");
        let args = vec![
            "update".to_string(),
            id.to_string(),
            "--set-metadata".to_string(),
            pair,
        ];
        let output = Self::run_json(repo, &args)?;
        parse_single_issue(&output, "bd update --set-metadata")
    }
}

fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.into_iter().map(str::to_string).collect()
}

fn display_command(repo: &Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 4);
    parts.push("bd".to_string());
    parts.push("-C".to_string());
    parts.push(repo.display().to_string());
    parts.extend(args.iter().cloned());
    parts.push("--json".to_string());
    parts.join(" ")
}

fn parse_issue_array(output: &str, command: &str) -> Result<Vec<Issue>> {
    serde_json::from_str(output).map_err(|e| BdError::json(command, &e))
}

fn parse_single_issue(output: &str, command: &str) -> Result<Issue> {
    let mut issues = parse_issue_array(output, command)?;
    match issues.len() {
        1 => Ok(issues.remove(0)),
        len => Err(BdError::new(format!(
            "expected one issue from `{command}`, got {len}"
        ))),
    }
}

fn parse_count(output: &str, command: &str) -> Result<u64> {
    let body: CountBody = serde_json::from_str(output).map_err(|e| BdError::json(command, &e))?;
    Ok(body.count)
}

fn parse_comment(output: &str, command: &str) -> Result<Comment> {
    serde_json::from_str(output).map_err(|e| BdError::json(command, &e))
}

fn parse_cli_error(output: &str) -> Option<CliErrorBody> {
    serde_json::from_str(output).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    const READY_WITH_ITEMS: &str = include_str!("../tests/fixtures/bd-ready-with-items.json");
    const SHOW_READY_META: &str = include_str!("../tests/fixtures/bd-show-ready-meta.json");
    const COUNT_WITH_ITEMS: &str = include_str!("../tests/fixtures/bd-count-with-items.json");
    const BLOCKED_WITH_ITEMS: &str = include_str!("../tests/fixtures/bd-blocked-with-items.json");
    const CLAIM_OUTPUT: &str = include_str!("../tests/fixtures/bd-claim-output.json");
    const RELEASE_OUTPUT: &str = include_str!("../tests/fixtures/bd-release-output.json");
    const SET_METADATA_OUTPUT: &str = include_str!("../tests/fixtures/bd-set-metadata-output.json");
    const COMMENT_OUTPUT: &str = include_str!("../tests/fixtures/bd-comment-output.json");
    const CLOSE_OUTPUT: &str = include_str!("../tests/fixtures/bd-close-output.json");
    const READY_ZERO_DRAINED: &str = include_str!("../tests/fixtures/bd-ready-zero-drained.json");
    const READY_ZERO_BLOCKED: &str = include_str!("../tests/fixtures/bd-ready-zero-blocked.json");
    const COUNT_ZERO: &str = include_str!("../tests/fixtures/bd-count-zero.json");
    const COUNT_ALL_BLOCKED: &str = include_str!("../tests/fixtures/bd-count-all-blocked.json");
    const BLOCKED_ZERO: &str = include_str!("../tests/fixtures/bd-blocked-zero.json");
    const BLOCKED_ALL_BLOCKED: &str = include_str!("../tests/fixtures/bd-blocked-all-blocked.json");
    const SHOW_BOGUS_ERROR: &str = include_str!("../tests/fixtures/bd-show-bogus-error.json");

    #[test]
    fn bd_client_parse_ready_reads_issue_fields_and_metadata() {
        let issues = parse_issue_array(READY_WITH_ITEMS, "bd ready").expect("ready fixture parses");

        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert_eq!(issue.id, "fixture-ready-meta");
        assert_eq!(issue.title, "ready with metadata");
        assert_eq!(issue.description, "ready issue description");
        assert_eq!(issue.acceptance_criteria, "ready acceptance");
        assert_eq!(
            issue.notes,
            "tier_floor: senior · complexity: S-M · verify_type: cargo"
        );
        assert_eq!(issue.status, "open");
        assert_eq!(issue.priority, 1);
        assert_eq!(issue.issue_type, "task");
        assert_eq!(issue.assignee, None);
        assert_eq!(issue.owner, "taylor.finklea@gmail.com");
        assert_eq!(issue.created_by, "Taylor Finklea");
        assert_eq!(
            issue.labels.as_deref(),
            Some(&["alpha".to_string(), "beta".to_string()][..])
        );
        assert_eq!(issue.estimated_minutes, Some(30));
        assert_eq!(issue.dependency_count, Some(0));
        assert_eq!(issue.dependent_count, Some(1));
        assert_eq!(issue.comment_count, Some(0));
        assert_eq!(
            issue
                .metadata
                .as_ref()
                .and_then(|m| m.get("tier_floor"))
                .and_then(serde_json::Value::as_str),
            Some("senior")
        );
    }

    #[test]
    fn bd_client_parse_show_returns_one_issue() {
        let issue = parse_single_issue(SHOW_READY_META, "bd show").expect("show fixture parses");

        assert_eq!(issue.id, "fixture-ready-meta");
    }

    #[test]
    fn bd_client_parse_count_reads_count_object() {
        let count = parse_count(COUNT_WITH_ITEMS, "bd count").expect("count fixture parses");

        assert_eq!(count, 3);
    }

    #[test]
    fn bd_client_parse_blocked_ignores_extra_blocker_fields() {
        let issues =
            parse_issue_array(BLOCKED_WITH_ITEMS, "bd blocked").expect("blocked fixture parses");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "fixture-blocked");
        assert_eq!(issues[0].metadata, None);
        assert_eq!(issues[0].dependencies, None);
    }

    #[test]
    fn bd_client_parse_claim_reads_claimed_issue() {
        let issue =
            parse_single_issue(CLAIM_OUTPUT, "bd update --claim").expect("claim fixture parses");

        assert_eq!(issue.status, "in_progress");
        assert_eq!(issue.assignee.as_deref(), Some("fixture-agent"));
        assert_eq!(issue.started_at.as_deref(), Some("2026-07-02T00:34:01Z"));
    }

    #[test]
    fn bd_client_parse_release_reads_open_unassigned_issue() {
        let issue = parse_single_issue(RELEASE_OUTPUT, "bd update release")
            .expect("release fixture parses");

        assert_eq!(issue.status, "open");
        assert_eq!(issue.assignee, None);
        assert_eq!(issue.started_at.as_deref(), Some("2026-07-02T00:34:01Z"));
    }

    #[test]
    fn bd_client_parse_set_metadata_reads_metadata_map() {
        let issue = parse_single_issue(SET_METADATA_OUTPUT, "bd update --set-metadata")
            .expect("metadata fixture parses");

        assert_eq!(
            issue
                .metadata
                .as_ref()
                .and_then(|m| m.get("verify_cmd"))
                .and_then(serde_json::Value::as_str),
            Some("cargo test bd_client")
        );
    }

    #[test]
    fn bd_client_parse_comment_reads_comment_shape() {
        let comment = parse_comment(COMMENT_OUTPUT, "bd comment").expect("comment fixture parses");

        assert_eq!(comment.issue_id, "fixture-claimed");
        assert_eq!(comment.text, "fixture comment");
    }

    #[test]
    fn bd_client_parse_close_reads_closed_issue() {
        let issue = parse_single_issue(CLOSE_OUTPUT, "bd close").expect("close fixture parses");

        assert_eq!(issue.status, "closed");
    }

    #[test]
    fn bd_client_parse_ready_zero_states_are_identical_empty_arrays() {
        let drained = parse_issue_array(READY_ZERO_DRAINED, "bd ready drained")
            .expect("drained ready fixture parses");
        let blocked = parse_issue_array(READY_ZERO_BLOCKED, "bd ready blocked")
            .expect("blocked ready fixture parses");

        assert!(drained.is_empty());
        assert!(blocked.is_empty());
    }

    #[test]
    fn bd_client_parse_count_and_blocked_distinguish_zero_states() {
        let drained_count =
            parse_count(COUNT_ZERO, "bd count empty").expect("zero count fixture parses");
        let all_blocked_count = parse_count(COUNT_ALL_BLOCKED, "bd count all blocked")
            .expect("all-blocked count fixture parses");
        let blocked_zero = parse_issue_array(BLOCKED_ZERO, "bd blocked empty")
            .expect("zero blocked fixture parses");
        let blocked_items = parse_issue_array(BLOCKED_ALL_BLOCKED, "bd blocked all blocked")
            .expect("all-blocked fixture parses");

        assert_eq!(drained_count, 0);
        assert_eq!(all_blocked_count, 2);
        assert!(blocked_zero.is_empty());
        assert_eq!(blocked_items.len(), 1);
    }

    #[test]
    fn bd_client_parse_error_shape_reads_json_error_body() {
        let err = parse_cli_error(SHOW_BOGUS_ERROR).expect("error fixture parses");

        assert_eq!(err.error, "no issues found matching the provided IDs");
        assert_eq!(err.schema_version, Some(1));
    }

    fn assert_bd_round_trip(temp: &Path) {
        let client = CommandBdClient::new();
        let ready = client.ready(temp).expect("ready works");
        assert_eq!(ready.len(), 1);
        assert_eq!(client.count(temp).expect("count works"), 2);
        assert_eq!(client.blocked(temp).expect("blocked works").len(), 1);
        assert_eq!(
            client
                .show(temp, "fixture-round-ready")
                .expect("show works")
                .id,
            "fixture-round-ready"
        );
        assert_eq!(
            client
                .claim(temp, "fixture-round-ready", "fixture-agent")
                .expect("claim works")
                .assignee
                .as_deref(),
            Some("fixture-agent")
        );
        assert_eq!(
            client
                .release(temp, "fixture-round-ready")
                .expect("release works")
                .status,
            "open"
        );
        assert_eq!(
            client
                .set_metadata(
                    temp,
                    "fixture-round-ready",
                    "verify_cmd",
                    "cargo test bd_client"
                )
                .expect("set metadata works")
                .metadata
                .as_ref()
                .and_then(|m| m.get("verify_cmd"))
                .and_then(serde_json::Value::as_str),
            Some("cargo test bd_client")
        );
        assert_eq!(
            client
                .comment(temp, "fixture-round-ready", "round comment")
                .expect("comment works")
                .text,
            "round comment"
        );
        assert_eq!(
            client
                .close(temp, "fixture-round-ready", "round close")
                .expect("close works")
                .status,
            "closed"
        );
    }

    #[test]
    fn bd_client_real_subprocess_round_trip_against_throwaway_repo() {
        if !bd_on_path() {
            return;
        }

        let temp = TempDir::new("bd-client-round-trip");
        init_bd_repo(temp.path());
        setup_issue(
            temp.path(),
            &[
                "create",
                "round ready",
                "--id",
                "fixture-round-ready",
                "--description",
                "round description",
                "--acceptance",
                "round acceptance",
                "--notes",
                "round notes",
                "-t",
                "task",
                "-p",
                "1",
                "--metadata",
                r#"{"tier_floor":"senior"}"#,
            ],
        );
        setup_issue(
            temp.path(),
            &[
                "create",
                "round blocked",
                "--id",
                "fixture-round-blocked",
                "--description",
                "blocked description",
                "--acceptance",
                "blocked acceptance",
                "-t",
                "task",
                "-p",
                "2",
                "--deps",
                "fixture-round-ready",
            ],
        );

        assert_bd_round_trip(temp.path());
    }

    #[test]
    fn bd_client_real_subprocess_conductor_revise_findings_round_trip_keeps_string_scalar_shape() {
        // Live-contract regression for conductor-0ya. A throwaway
        // `bd` repo proved `bd update --set-metadata` returns the
        // stored value as a JSON string scalar, not a native array,
        // even when the caller wrote a JSON-encoded array. This test
        // pins that contract: set the metadata through the real
        // `CommandBdClient`, read it back via `show`, and assert the
        // value is the JSON string literal, not a native
        // `Value::Array`. Dispatch has to accept this shape; if the
        // live bd ever flips back to native arrays, the parser will
        // fail closed and this test will point at the change.
        if !bd_on_path() {
            return;
        }

        let temp = TempDir::new("bd-client-revise-findings");
        init_bd_repo(temp.path());
        setup_issue(
            temp.path(),
            &[
                "create",
                "revise findings round trip",
                "--id",
                "fixture-revise-findings",
                "--description",
                "revise round trip description",
                "--acceptance",
                "revise round trip acceptance",
                "--notes",
                "tier_floor: senior",
                "-t",
                "task",
                "-p",
                "1",
            ],
        );

        let client = CommandBdClient::new();
        let findings = serde_json::Value::Array(vec![
            serde_json::Value::String("missing edge-case test".to_string()),
            serde_json::Value::String("scope drift".to_string()),
        ])
        .to_string();
        let set = client
            .set_metadata(
                temp.path(),
                "fixture-revise-findings",
                "conductor_revise_findings",
                &findings,
            )
            .expect("set_metadata works against the live bd");
        let stored = set
            .metadata
            .as_ref()
            .and_then(|m| m.get("conductor_revise_findings"))
            .expect("conductor_revise_findings lives in returned metadata");
        assert_eq!(
            stored,
            &serde_json::Value::String(findings.clone()),
            "live bd must round-trip the value as a JSON string scalar"
        );

        let ready_issue = client
            .ready(temp.path())
            .expect("ready works")
            .into_iter()
            .find(|issue| issue.id == "fixture-revise-findings")
            .expect("ready returns the issue");
        let ready_value = ready_issue
            .metadata
            .as_ref()
            .and_then(|m| m.get("conductor_revise_findings"))
            .expect("ready carries the metadata");
        assert_eq!(
            ready_value,
            &serde_json::Value::String(findings.clone()),
            "bd ready must also surface the JSON string scalar shape"
        );

        let shown = client
            .show(temp.path(), "fixture-revise-findings")
            .expect("show works");
        let shown_value = shown
            .metadata
            .as_ref()
            .and_then(|m| m.get("conductor_revise_findings"))
            .expect("show carries the metadata");
        assert_eq!(
            shown_value,
            &serde_json::Value::String(findings.clone()),
            "bd show must surface the JSON string scalar shape"
        );

        // The stored scalar must be the exact JSON string the
        // caller wrote; if a future bd change adds escapes or
        // whitespace, the dispatch parser will silently drop the
        // findings, so this assertion is the tripwire.
        let stored_string = shown_value
            .as_str()
            .expect("stored value is a JSON string scalar");
        let reparsed: Vec<String> = serde_json::from_str(stored_string)
            .expect("stored JSON string scalar parses back to a string array");
        assert_eq!(
            reparsed,
            vec!["missing edge-case test".to_string(), "scope drift".to_string()],
            "the live round-trip must preserve the exact findings array"
        );
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp repo");
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

    fn bd_on_path() -> bool {
        Command::new("which")
            .arg("bd")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn init_bd_repo(repo: &Path) {
        let output = Command::new("bd")
            .current_dir(repo)
            .args(["init", "--non-interactive", "-p", "fixture"])
            .stdin(Stdio::null())
            .output()
            .expect("spawn bd init");
        assert!(
            output.status.success(),
            "bd init failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup_issue(repo: &Path, args: &[&str]) {
        let output = Command::new("bd")
            .arg("-C")
            .arg(repo)
            .args(args)
            .arg("--json")
            .stdin(Stdio::null())
            .output()
            .expect("spawn bd setup command");
        assert!(
            output.status.success(),
            "bd setup failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
