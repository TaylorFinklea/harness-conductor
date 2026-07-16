//! `conductor/run@1` manifest + `conductor/event@1` JSONL run artifacts.
//!
//! Every run lives under `<state_dir>/runs/<run-id>/`: a whole-file atomic
//! `manifest.json` (mirrors `ratchet.rs::save`'s sibling-temp-then-rename
//! replace) pinning target, job, approved profile envelope, Bursar roster
//! artifact hash, limits, artifacts, lifecycle, and final outcome, plus an
//! append-only `events.jsonl` (mirrors `ledger.rs::append_serialized`'s
//! read-modify-write-then-rename replace) recording one stable-schema event
//! per attempt, verifier, review, coverage gap, and terminal outcome.

#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema tag stamped on every manifest written by this module.
pub(crate) const RUN_SCHEMA: &str = "conductor/run@1";
/// Schema tag stamped on every event line written by this module.
pub(crate) const EVENT_SCHEMA: &str = "conductor/event@1";

pub(crate) type Result<T> = std::result::Result<T, RunError>;

/// Error returned by run-artifact reads and writes.
#[derive(Debug, Clone)]
pub(crate) struct RunError {
    message: String,
}

impl RunError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunError {}

/// The closed job kinds from the core-consolidation spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunJob {
    Work,
    Review,
    Consult,
    Arena,
}

/// Run lifecycle state pinned on the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunLifecycle {
    Started,
    Running,
    Finished,
}

/// One event kind from the spec's stable `conductor/event@1` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    RunStarted,
    AttemptStarted,
    AttemptFinished,
    VerifyFinished,
    ReviewFinished,
    RunFinished,
    CoverageGap,
}

/// `{"path": ..., "sha256": ...}` artifact identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactRef {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// `{"repo": ..., "bead": ...}` run/event target identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct RunTarget {
    pub(crate) repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bead: Option<String>,
}

/// Approved profile/fallback envelope pinned into the manifest at run start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct ApprovedProfileEnvelope {
    pub(crate) profiles: Vec<String>,
}

/// Runtime limits pinned into the manifest at run start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct RunLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) item_wall_clock_mins: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_attempts: Option<u64>,
}

/// `conductor/run@1` — the atomic, versioned run manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunManifest {
    pub(crate) schema: String,
    pub(crate) run_id: String,
    pub(crate) job: RunJob,
    pub(crate) target: RunTarget,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) approved_profiles: ApprovedProfileEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bursar_roster_artifact: Option<ArtifactRef>,
    pub(crate) limits: RunLimits,
    #[serde(default)]
    pub(crate) artifacts: Vec<ArtifactRef>,
    pub(crate) lifecycle: RunLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<String>,
}

/// `conductor/event@1` — one append-only event line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunEvent {
    pub(crate) schema: String,
    pub(crate) event_id: String,
    pub(crate) run_id: String,
    pub(crate) seq: u64,
    pub(crate) ts: String,
    pub(crate) kind: EventKind,
    pub(crate) job: RunJob,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile_id: Option<String>,
    pub(crate) target: RunTarget,
    #[serde(default)]
    pub(crate) artifact_refs: Vec<ArtifactRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<String>,
}

/// Fields pinned into a new run's manifest at creation.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewRun {
    pub(crate) target: RunTarget,
    pub(crate) approved_profiles: Vec<String>,
    pub(crate) bursar_roster_artifact: Option<ArtifactRef>,
    pub(crate) limits: RunLimits,
}

/// Fields for one `conductor/event@1` row; `run_id`, `seq`, `ts`, `job`, and
/// `target` are filled in by the owning [`RunHandle`].
#[derive(Debug, Clone, Default)]
pub(crate) struct EventInput {
    pub(crate) profile_id: Option<String>,
    pub(crate) artifact_refs: Vec<ArtifactRef>,
    pub(crate) outcome: Option<String>,
}

/// Handle to one created (or reopened) run directory; owns the manifest and
/// the append-only event log's next sequence number.
pub(crate) struct RunHandle {
    dir: PathBuf,
    manifest: RunManifest,
    next_seq: u64,
}

/// Monotonic in-process disambiguator so run ids generated within the same
/// wall-clock second (even the same nanosecond, on coarse-resolution clocks)
/// never collide. Correctness does not depend on clock resolution or entropy.
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_run_id(job: RunJob, now: DateTime<Utc>) -> String {
    let counter = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "run-{}-{}-{counter:06}",
        job_label(job),
        now.format("%Y%m%dT%H%M%S%.9f")
    )
}

const fn job_label(job: RunJob) -> &'static str {
    match job {
        RunJob::Work => "work",
        RunJob::Review => "review",
        RunJob::Consult => "consult",
        RunJob::Arena => "arena",
    }
}

impl RunHandle {
    /// Creates a new run directory under `<state_dir>/runs/<run-id>/` and
    /// writes the initial `manifest.json`.
    pub(crate) fn create(state_dir: &Path, job: RunJob, request: NewRun) -> Result<Self> {
        Self::create_at(state_dir, job, request, Utc::now())
    }

    fn create_at(
        state_dir: &Path,
        job: RunJob,
        request: NewRun,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        let run_id = new_run_id(job, now);
        let dir = runs_dir(state_dir).join(&run_id);
        std::fs::create_dir_all(&dir).map_err(|e| {
            RunError::new(format!("failed to create run dir {}: {e}", dir.display()))
        })?;
        let created_at = now.to_rfc3339();
        let manifest = RunManifest {
            schema: RUN_SCHEMA.to_string(),
            run_id,
            job,
            target: request.target,
            created_at: created_at.clone(),
            updated_at: created_at,
            approved_profiles: ApprovedProfileEnvelope {
                profiles: request.approved_profiles,
            },
            bursar_roster_artifact: request.bursar_roster_artifact,
            limits: request.limits,
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        };
        let handle = Self {
            dir,
            manifest,
            next_seq: 1,
        };
        handle.write_manifest()?;
        Ok(handle)
    }

    /// Reopens an existing run directory, validating the manifest schema and
    /// resuming the event sequence counter after the last recorded event.
    pub(crate) fn open(state_dir: &Path, run_id: &str) -> Result<Self> {
        let dir = runs_dir(state_dir).join(run_id);
        let manifest = read_manifest(&dir.join("manifest.json"))?;
        let events_path = dir.join("events.jsonl");
        let next_seq = if events_path.exists() {
            read_events(&events_path)?
                .last()
                .map_or(1, |event| event.seq + 1)
        } else {
            1
        };
        Ok(Self {
            dir,
            manifest,
            next_seq,
        })
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.manifest.run_id
    }

    pub(crate) fn manifest(&self) -> &RunManifest {
        &self.manifest
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    pub(crate) fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// Appends one stable-schema event and updates the manifest's lifecycle,
    /// `updated_at`, and (for `run_finished`) final `outcome`.
    pub(crate) fn append_event(&mut self, kind: EventKind, input: EventInput) -> Result<()> {
        self.append_event_at(kind, input, Utc::now())
    }

    fn append_event_at(
        &mut self,
        kind: EventKind,
        input: EventInput,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let seq = self.next_seq;
        let event = RunEvent {
            schema: EVENT_SCHEMA.to_string(),
            event_id: format!("{}-{seq:06}", self.manifest.run_id),
            run_id: self.manifest.run_id.clone(),
            seq,
            ts: now.to_rfc3339(),
            kind,
            job: self.manifest.job,
            profile_id: input.profile_id,
            target: self.manifest.target.clone(),
            artifact_refs: input.artifact_refs,
            outcome: input.outcome.clone(),
        };
        append_event_line(&self.events_path(), &event)?;
        self.next_seq += 1;

        if matches!(kind, EventKind::RunFinished) {
            self.manifest.lifecycle = RunLifecycle::Finished;
            self.manifest.outcome = input.outcome;
        } else if matches!(self.manifest.lifecycle, RunLifecycle::Started) {
            self.manifest.lifecycle = RunLifecycle::Running;
        }
        self.manifest.updated_at = now.to_rfc3339();
        self.write_manifest()
    }

    /// Records the terminal `run_finished` event and pins the final outcome.
    pub(crate) fn finish(&mut self, outcome: impl Into<String>) -> Result<()> {
        self.append_event(
            EventKind::RunFinished,
            EventInput {
                outcome: Some(outcome.into()),
                ..EventInput::default()
            },
        )
    }

    fn write_manifest(&self) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|e| RunError::new(format!("failed to serialize run manifest: {e}")))?;
        bytes.push(b'\n');
        atomic_replace(&self.manifest_path(), &bytes)
    }
}

/// Returns `<state_dir>/runs`.
pub(crate) fn runs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("runs")
}

/// Reads and validates `manifest.json`, rejecting an unknown schema before
/// attempting to interpret the rest of the shape (a future/foreign schema
/// version may not share this struct's fields at all).
pub(crate) fn read_manifest(path: &Path) -> Result<RunManifest> {
    let bytes = std::fs::read(path)
        .map_err(|e| RunError::new(format!("failed to read manifest {}: {e}", path.display())))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| RunError::new(format!("failed to parse manifest {}: {e}", path.display())))?;
    check_schema(&value, RUN_SCHEMA, path)?;
    serde_json::from_value(value)
        .map_err(|e| RunError::new(format!("failed to parse manifest {}: {e}", path.display())))
}

fn check_schema(value: &serde_json::Value, expected: &str, path: &Path) -> Result<()> {
    let schema = value.get("schema").and_then(serde_json::Value::as_str);
    if schema != Some(expected) {
        return Err(RunError::new(format!(
            "unknown schema {:?} in {}, expected {expected:?}",
            schema.unwrap_or("<missing>"),
            path.display()
        )));
    }
    Ok(())
}

/// Reads and validates every line of `events.jsonl`, rejecting an unknown
/// schema or a malformed (e.g. partially written) line. Fails closed on the
/// first bad line rather than silently dropping it.
pub(crate) fn read_events(path: &Path) -> Result<Vec<RunEvent>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RunError::new(format!("failed to read events {}: {e}", path.display())))?;
    let mut events = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            RunError::new(format!(
                "{} line {}: malformed event (partial write?): {e}",
                path.display(),
                idx + 1
            ))
        })?;
        check_schema(&value, EVENT_SCHEMA, path)
            .map_err(|e| RunError::new(format!("{e} (line {})", idx + 1)))?;
        let event: RunEvent = serde_json::from_value(value).map_err(|e| {
            RunError::new(format!(
                "{} line {}: malformed event (partial write?): {e}",
                path.display(),
                idx + 1
            ))
        })?;
        events.push(event);
    }
    Ok(events)
}

/// Whole-file atomic replace: write to a sibling temp file, then rename over
/// the original. Mirrors `ratchet.rs::save`.
fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            RunError::new(format!("failed to create dir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| RunError::new(format!("failed to write temp {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RunError::new(format!(
            "failed to rename temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

/// Append-only atomic replace: read the existing file, append the new line
/// in memory, write the full new contents to a sibling temp file, then
/// rename over the original. Mirrors `ledger.rs::append_serialized`.
fn append_event_line(path: &Path, event: &RunEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            RunError::new(format!("failed to create dir {}: {e}", parent.display()))
        })?;
    }
    let mut new_line = serde_json::to_vec(event)
        .map_err(|e| RunError::new(format!("failed to serialize event: {e}")))?;
    new_line.push(b'\n');

    let existing = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(RunError::new(format!(
                "failed to read events {}: {e}",
                path.display()
            )));
        }
    };
    let mut contents = existing;
    contents.extend_from_slice(&new_line);

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &contents)
        .map_err(|e| RunError::new(format!("failed to write temp {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RunError::new(format!(
            "failed to rename temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-run-{label}-{nanos}"));
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

    fn fixed_now() -> DateTime<Utc> {
        "2026-07-16T12:00:00Z".parse().expect("fixed timestamp")
    }

    fn new_run_request() -> NewRun {
        NewRun {
            target: RunTarget {
                repo: "/repo/conductor".to_string(),
                bead: Some("conductor-run-contract".to_string()),
            },
            approved_profiles: vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()],
            bursar_roster_artifact: Some(ArtifactRef {
                path: "/home/.config/bursar/roster.toml".to_string(),
                sha256: "a".repeat(64),
            }),
            limits: RunLimits {
                item_wall_clock_mins: Some(45),
                max_attempts: Some(3),
            },
        }
    }

    #[test]
    fn run_event_manifest_pins_target_job_profiles_roster_hash_limits_and_lifecycle() {
        let temp = TempDir::new("manifest-pins");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");

        let manifest = read_manifest(&handle.manifest_path()).expect("read manifest");
        assert_eq!(manifest.schema, RUN_SCHEMA);
        assert_eq!(manifest.job, RunJob::Work);
        assert_eq!(manifest.target.repo, "/repo/conductor");
        assert_eq!(
            manifest.target.bead.as_deref(),
            Some("conductor-run-contract")
        );
        assert_eq!(
            manifest.approved_profiles.profiles,
            vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()]
        );
        assert_eq!(
            manifest
                .bursar_roster_artifact
                .as_ref()
                .map(|a| a.sha256.clone()),
            Some("a".repeat(64))
        );
        assert_eq!(manifest.limits.item_wall_clock_mins, Some(45));
        assert_eq!(manifest.limits.max_attempts, Some(3));
        assert_eq!(manifest.lifecycle, RunLifecycle::Started);
        assert!(manifest.outcome.is_none());
    }

    #[test]
    fn run_event_kinds_cover_attempt_verify_review_coverage_gap_and_terminal_outcome() {
        let temp = TempDir::new("event-kinds");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");

        for kind in [
            EventKind::AttemptStarted,
            EventKind::AttemptFinished,
            EventKind::VerifyFinished,
            EventKind::ReviewFinished,
            EventKind::CoverageGap,
        ] {
            handle
                .append_event_at(
                    kind,
                    EventInput {
                        profile_id: Some("claude-sonnet-5".to_string()),
                        ..EventInput::default()
                    },
                    fixed_now(),
                )
                .expect("append event");
        }
        handle.finish("verified").expect("finish run");

        let events = read_events(&handle.events_path()).expect("read events");
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::AttemptStarted,
                EventKind::AttemptFinished,
                EventKind::VerifyFinished,
                EventKind::ReviewFinished,
                EventKind::CoverageGap,
                EventKind::RunFinished,
            ]
        );
        assert!(events.iter().all(|e| e.schema == EVENT_SCHEMA));
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);

        let manifest = read_manifest(&handle.manifest_path()).expect("read manifest");
        assert_eq!(manifest.lifecycle, RunLifecycle::Finished);
        assert_eq!(manifest.outcome.as_deref(), Some("verified"));
    }

    #[test]
    fn run_event_rejects_unknown_manifest_schema() {
        let temp = TempDir::new("bad-manifest-schema");
        let path = temp.path().join("manifest.json");
        // Otherwise-complete manifest so the failure is unambiguously the
        // schema check, not a missing-field parse error.
        let mut manifest = serde_json::to_value(RunManifest {
            schema: "conductor/run@2".to_string(),
            run_id: "x".to_string(),
            job: RunJob::Work,
            target: RunTarget {
                repo: "/repo".to_string(),
                bead: None,
            },
            created_at: "2026-07-16T12:00:00Z".to_string(),
            updated_at: "2026-07-16T12:00:00Z".to_string(),
            approved_profiles: ApprovedProfileEnvelope::default(),
            bursar_roster_artifact: None,
            limits: RunLimits::default(),
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        })
        .unwrap();
        manifest["schema"] = serde_json::json!("conductor/run@2");
        std::fs::write(&path, manifest.to_string()).unwrap();

        let err = read_manifest(&path).expect_err("unknown schema must fail closed");
        assert!(err.to_string().contains("unknown schema"));
    }

    #[test]
    fn run_event_rejects_unknown_event_schema() {
        let temp = TempDir::new("bad-event-schema");
        let path = temp.path().join("events.jsonl");
        let bad_line = serde_json::json!({
            "schema": "conductor/event@2",
            "event_id": "x-1",
            "run_id": "x",
            "seq": 1,
            "ts": "2026-07-16T12:00:00Z",
            "kind": "run_started",
            "job": "work",
            "target": {"repo": "/repo"},
        });
        std::fs::write(&path, format!("{bad_line}\n")).unwrap();

        let err = read_events(&path).expect_err("unknown schema must fail closed");
        assert!(err.to_string().contains("unknown schema"));
    }

    #[test]
    fn run_event_detects_partial_write() {
        let temp = TempDir::new("partial-write");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::AttemptStarted,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append first event");

        // Simulate a crash mid-write: a truncated JSON line appended directly,
        // bypassing the atomic append helper.
        let mut raw = std::fs::read_to_string(handle.events_path()).unwrap();
        raw.push_str("{\"schema\":\"conductor/event@1\",\"event_id\":\"trunc");
        std::fs::write(handle.events_path(), raw).unwrap();

        let err = read_events(&handle.events_path()).expect_err("partial line must fail closed");
        assert!(err.to_string().contains("malformed event"));
    }

    #[test]
    fn run_event_run_ids_do_not_collide_within_the_same_second() {
        let now = fixed_now();
        let mut ids = HashSet::new();
        for _ in 0..500 {
            assert!(ids.insert(new_run_id(RunJob::Work, now)), "run id collided");
        }
    }

    #[test]
    fn run_event_manifest_and_events_writes_leave_no_temp_file_behind() {
        let temp = TempDir::new("no-temp-leftover");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Review, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::VerifyFinished,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append event");
        handle.finish("passed").expect("finish run");

        assert!(!handle.manifest_path().with_extension("json.tmp").exists());
        assert!(!handle.events_path().with_extension("json.tmp").exists());
        assert!(handle.manifest_path().is_file());
        assert!(handle.events_path().is_file());
    }

    #[test]
    fn run_event_open_resumes_sequence_and_rejects_unknown_schema_on_reopen() {
        let temp = TempDir::new("reopen");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Arena, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::AttemptStarted,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append event");
        let run_id = handle.run_id().to_string();
        drop(handle);

        let mut reopened = RunHandle::open(temp.path(), &run_id).expect("reopen run");
        reopened
            .append_event_at(
                EventKind::AttemptFinished,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append second event");
        let events = read_events(&reopened.events_path()).expect("read events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].seq, 2);

        // Corrupt the manifest after the fact and confirm reopen fails closed.
        std::fs::write(reopened.manifest_path(), br#"{"schema":"conductor/run@9"}"#).unwrap();
        assert!(RunHandle::open(temp.path(), &run_id).is_err());
    }

    #[test]
    fn run_event_run_dir_is_collision_resistant_under_state_dir() {
        let temp = TempDir::new("run-dir-layout");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Consult, new_run_request(), fixed_now())
                .expect("create run");
        assert!(
            handle
                .manifest_path()
                .starts_with(runs_dir(temp.path()).join(handle.run_id()))
        );
        assert!(handle.run_id().starts_with("run-consult-"));
    }
}
