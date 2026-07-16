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
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

    pub(crate) fn into_message(self) -> String {
        self.message
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

/// Verifier configuration pinned into the manifest before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct RunVerifier {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mechanical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) qualitative: Option<String>,
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
    pub(crate) verifier: RunVerifier,
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
    pub(crate) verifier: RunVerifier,
    pub(crate) approval: Option<serde_json::Value>,
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

/// Monotonic per-process disambiguator used in addition to the process id and
/// exclusive directory creation. Correctness comes from `create_dir`, not the
/// clock or this counter.
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_run_id(job: RunJob, now: DateTime<Utc>, counter: u64) -> String {
    format!(
        "run-{}-{}-p{}-{counter:06}",
        job_label(job),
        now.format("%Y%m%dT%H%M%S%.9f"),
        std::process::id(),
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
        if let Some(artifact) = request.bursar_roster_artifact.as_ref() {
            validate_artifact_ref(artifact, "bursar roster artifact")?;
        }
        let root = runs_dir(state_dir);
        std::fs::create_dir_all(&root).map_err(|e| {
            RunError::new(format!("failed to create runs dir {}: {e}", root.display()))
        })?;
        let (run_id, dir) = loop {
            let counter = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
            let run_id = new_run_id(job, now, counter);
            let dir = root.join(&run_id);
            match std::fs::create_dir(&dir) {
                Ok(()) => break (run_id, dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(RunError::new(format!(
                        "failed to create run dir {}: {error}",
                        dir.display()
                    )));
                }
            }
        };
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
            verifier: request.verifier,
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        };
        let mut handle = Self {
            dir,
            manifest,
            next_seq: 1,
        };
        let cleanup_dir = handle.dir.clone();
        let setup = (|| {
            std::fs::create_dir(handle.dir.join("attempts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create attempts dir {}: {error}",
                    handle.dir.join("attempts").display()
                ))
            })?;
            std::fs::create_dir(handle.dir.join("artifacts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create artifacts dir {}: {error}",
                    handle.dir.join("artifacts").display()
                ))
            })?;
            let mut initial_refs = Vec::new();
            if let Some(approval) = request.approval.as_ref() {
                initial_refs.push(handle.write_approval(approval)?);
            }
            if let Some(roster) = handle.manifest.bursar_roster_artifact.clone() {
                initial_refs.push(roster);
            }
            handle.write_manifest()?;
            handle.append_event_at(
                EventKind::RunStarted,
                EventInput {
                    artifact_refs: initial_refs,
                    outcome: Some("started".to_string()),
                    ..EventInput::default()
                },
                now,
            )?;
            if handle.manifest.bursar_roster_artifact.is_none() {
                handle.append_event_at(
                    EventKind::CoverageGap,
                    EventInput {
                        outcome: Some("bursar_roster_artifact_unavailable".to_string()),
                        ..EventInput::default()
                    },
                    now,
                )?;
            }
            Ok(handle)
        })();
        if setup.is_err() {
            let _ = std::fs::remove_dir_all(cleanup_dir);
        }
        setup
    }

    /// Reopens an existing run directory, validating the manifest schema and
    /// resuming the event sequence counter after the last recorded event.
    pub(crate) fn open(state_dir: &Path, run_id: &str) -> Result<Self> {
        validate_run_id(run_id)?;
        let dir = runs_dir(state_dir).join(run_id);
        let manifest = read_manifest(&dir.join("manifest.json"))?;
        if manifest.run_id != run_id {
            return Err(RunError::new(format!(
                "manifest run_id {:?} does not match directory {run_id:?}",
                manifest.run_id
            )));
        }
        let events_path = dir.join("events.jsonl");
        let events = read_events(&events_path)?;
        if events.is_empty() {
            return Err(RunError::new("run event log is empty"));
        }
        for event in &events {
            if event.run_id != manifest.run_id
                || event.job != manifest.job
                || event.target != manifest.target
            {
                return Err(RunError::new(format!(
                    "event identity does not match manifest at sequence {}",
                    event.seq
                )));
            }
        }
        let last = events.last().expect("non-empty events checked above");
        if matches!(manifest.lifecycle, RunLifecycle::Finished)
            != matches!(last.kind, EventKind::RunFinished)
        {
            return Err(RunError::new(
                "manifest lifecycle does not match terminal event state",
            ));
        }
        if matches!(last.kind, EventKind::RunFinished) && manifest.outcome != last.outcome {
            return Err(RunError::new(
                "manifest outcome does not match terminal event outcome",
            ));
        }
        let next_seq = last.seq + 1;
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

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copies an existing output into this run using create-new semantics and
    /// returns its content-addressed identity.
    pub(crate) fn capture_artifact(
        &self,
        source: &Path,
        relative_destination: &Path,
    ) -> Result<ArtifactRef> {
        validate_relative_artifact_path(relative_destination)?;
        let bytes = std::fs::read(source).map_err(|error| {
            RunError::new(format!(
                "failed to read artifact {}: {error}",
                source.display()
            ))
        })?;
        let destination = self.dir.join(relative_destination);
        write_new_file(&destination, &bytes)?;
        Ok(artifact_ref(relative_destination, &bytes))
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
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new("cannot append to a finished run"));
        }
        for artifact in &input.artifact_refs {
            validate_artifact_ref(artifact, "event artifact")?;
        }
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

        for artifact in &event.artifact_refs {
            if !self.manifest.artifacts.contains(artifact) {
                self.manifest.artifacts.push(artifact.clone());
            }
        }

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
        self.finish_with_artifacts(outcome, Vec::new())
    }

    pub(crate) fn finish_with_artifacts(
        &mut self,
        outcome: impl Into<String>,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<()> {
        self.append_event(
            EventKind::RunFinished,
            EventInput {
                outcome: Some(outcome.into()),
                artifact_refs,
                profile_id: None,
            },
        )
    }

    fn write_approval(&self, approval: &serde_json::Value) -> Result<ArtifactRef> {
        let mut bytes = serde_json::to_vec_pretty(approval)
            .map_err(|error| RunError::new(format!("failed to serialize approval: {error}")))?;
        bytes.push(b'\n');
        let relative = Path::new("approval.json");
        write_new_file(&self.dir.join(relative), &bytes)?;
        Ok(artifact_ref(relative, &bytes))
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
    let manifest: RunManifest = serde_json::from_value(value)
        .map_err(|e| RunError::new(format!("failed to parse manifest {}: {e}", path.display())))?;
    validate_run_id(&manifest.run_id)?;
    if let Some(artifact) = manifest.bursar_roster_artifact.as_ref() {
        validate_artifact_ref(artifact, "manifest bursar roster artifact")?;
    }
    for artifact in &manifest.artifacts {
        validate_artifact_ref(artifact, "manifest artifact")?;
        validate_local_artifact(path, artifact)?;
    }
    Ok(manifest)
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
    let bytes = std::fs::read(path)
        .map_err(|e| RunError::new(format!("failed to read events {}: {e}", path.display())))?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(RunError::new(format!(
            "{}: malformed event (partial final line)",
            path.display()
        )));
    }
    let content = std::str::from_utf8(&bytes).map_err(|error| {
        RunError::new(format!(
            "{}: malformed event log encoding: {error}",
            path.display()
        ))
    })?;
    let mut events = Vec::new();
    let mut identity: Option<(String, RunJob, RunTarget)> = None;
    for (idx, line) in content.split_terminator('\n').enumerate() {
        if line.trim().is_empty() {
            return Err(RunError::new(format!(
                "{} line {}: blank event line",
                path.display(),
                idx + 1
            )));
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
        let expected_seq =
            u64::try_from(idx).map_err(|_| RunError::new("event sequence exceeds u64"))? + 1;
        if event.seq != expected_seq {
            return Err(RunError::new(format!(
                "{} line {}: event sequence gap, expected {expected_seq}, found {}",
                path.display(),
                idx + 1,
                event.seq
            )));
        }
        let expected_event_id = format!("{}-{expected_seq:06}", event.run_id);
        if event.event_id != expected_event_id {
            return Err(RunError::new(format!(
                "{} line {}: event_id {:?} does not match {:?}",
                path.display(),
                idx + 1,
                event.event_id,
                expected_event_id
            )));
        }
        validate_run_id(&event.run_id)?;
        for artifact in &event.artifact_refs {
            validate_artifact_ref(artifact, "event artifact")?;
            validate_local_artifact(path, artifact)?;
        }
        match &identity {
            None => {
                if !matches!(event.kind, EventKind::RunStarted) {
                    return Err(RunError::new(format!(
                        "{} line 1: first event must be run_started",
                        path.display()
                    )));
                }
                identity = Some((event.run_id.clone(), event.job, event.target.clone()));
            }
            Some((run_id, job, target)) => {
                if event.run_id != *run_id || event.job != *job || event.target != *target {
                    return Err(RunError::new(format!(
                        "{} line {}: event run_id/job/target identity mismatch",
                        path.display(),
                        idx + 1
                    )));
                }
            }
        }
        events.push(event);
    }
    Ok(events)
}

fn validate_run_id(run_id: &str) -> Result<()> {
    let mut components = Path::new(run_id).components();
    if run_id.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(RunError::new(format!("invalid run_id {run_id:?}")));
    }
    Ok(())
}

fn validate_artifact_ref(artifact: &ArtifactRef, label: &str) -> Result<()> {
    if artifact.path.trim().is_empty() {
        return Err(RunError::new(format!("{label} has an empty path")));
    }
    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunError::new(format!(
            "{label} has malformed sha256 {:?}",
            artifact.sha256
        )));
    }
    Ok(())
}

fn validate_local_artifact(contract_path: &Path, artifact: &ArtifactRef) -> Result<()> {
    let relative = Path::new(&artifact.path);
    let is_local = relative == Path::new("approval.json")
        || relative.starts_with("attempts")
        || relative.starts_with("artifacts");
    if !is_local {
        return Ok(());
    }
    validate_relative_artifact_path(relative)?;
    let run_dir = contract_path
        .parent()
        .ok_or_else(|| RunError::new("contract path has no run directory"))?;
    let bytes = std::fs::read(run_dir.join(relative)).map_err(|error| {
        RunError::new(format!(
            "failed to read referenced artifact {}: {error}",
            relative.display()
        ))
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != artifact.sha256 {
        return Err(RunError::new(format!(
            "artifact hash mismatch for {}",
            relative.display()
        )));
    }
    Ok(())
}

fn validate_relative_artifact_path(path: &Path) -> Result<()> {
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(RunError::new(format!(
                    "artifact destination must be relative and contained: {}",
                    path.display()
                )));
            }
        }
    }
    if !saw_component {
        return Err(RunError::new("artifact destination must not be empty"));
    }
    Ok(())
}

fn artifact_ref(path: &Path, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        path: path.to_string_lossy().replace('\\', "/"),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            RunError::new(format!(
                "failed to create artifact dir {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(path).map_err(|error| {
        RunError::new(format!(
            "failed to create immutable artifact {}: {error}",
            path.display()
        ))
    })?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(RunError::new(format!(
            "failed to write immutable artifact {}: {error}",
            path.display()
        )));
    }
    Ok(())
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
    use std::process::{Command, Stdio};
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
            verifier: RunVerifier {
                mechanical: Some("cargo test".to_string()),
                qualitative: Some("lead-review".to_string()),
            },
            approval: Some(serde_json::json!({
                "schema": "test/approval@1",
                "decision": "approved"
            })),
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
        assert_eq!(manifest.verifier.mechanical.as_deref(), Some("cargo test"));
        assert_eq!(manifest.lifecycle, RunLifecycle::Running);
        assert!(manifest.outcome.is_none());
        assert!(handle.dir().join("approval.json").is_file());
        assert!(handle.dir().join("attempts").is_dir());
        assert!(handle.dir().join("artifacts").is_dir());

        let events = read_events(&handle.events_path()).expect("read initial events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::RunStarted);
        assert_eq!(events[0].artifact_refs[0].path, "approval.json");
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
                EventKind::RunStarted,
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
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7]);

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
            verifier: RunVerifier::default(),
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
        for counter in 0..500 {
            assert!(
                ids.insert(new_run_id(RunJob::Work, now, counter)),
                "run id collided"
            );
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
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].seq, 3);

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
        assert!(handle.dir().join("attempts").is_dir());
        assert!(handle.dir().join("artifacts").is_dir());
    }

    #[test]
    fn run_event_missing_bursar_roster_emits_explicit_coverage_gap() {
        let temp = TempDir::new("bursar-gap");
        let mut request = new_run_request();
        request.bursar_roster_artifact = None;
        let handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create run");

        let events = read_events(&handle.events_path()).expect("read events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::RunStarted);
        assert_eq!(events[1].kind, EventKind::CoverageGap);
        assert_eq!(
            events[1].outcome.as_deref(),
            Some("bursar_roster_artifact_unavailable")
        );
    }

    #[test]
    fn run_event_approval_and_captured_artifacts_are_immutable_and_hashed() {
        let temp = TempDir::new("immutable-artifacts");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let source = temp.path().join("source.log");
        std::fs::write(&source, b"artifact bytes\n").expect("write source");
        let relative = Path::new("attempts/001/stdout.log");

        let artifact = handle
            .capture_artifact(&source, relative)
            .expect("capture artifact");
        assert_eq!(artifact.path, "attempts/001/stdout.log");
        assert_eq!(artifact.sha256.len(), 64);
        assert!(handle.capture_artifact(&source, relative).is_err());
        assert!(
            handle
                .capture_artifact(&source, Path::new("approval.json"))
                .is_err()
        );
        std::fs::write(handle.dir().join("approval.json"), b"tampered\n").expect("tamper approval");
        assert!(read_manifest(&handle.manifest_path()).is_err());
        assert!(read_events(&handle.events_path()).is_err());
    }

    #[test]
    fn run_event_read_rejects_sequence_and_identity_corruption() {
        for corruption in ["seq", "event_id", "run_id", "job", "target"] {
            let temp = TempDir::new(corruption);
            let mut handle =
                RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                    .expect("create run");
            handle
                .append_event_at(
                    EventKind::AttemptStarted,
                    EventInput::default(),
                    fixed_now(),
                )
                .expect("append event");
            let path = handle.events_path();
            let mut rows = event_values(&path);
            match corruption {
                "seq" => {
                    rows[1]["seq"] = serde_json::json!(3);
                    rows[1]["event_id"] = serde_json::json!(format!("{}-000003", handle.run_id()));
                }
                "event_id" => rows[1]["event_id"] = serde_json::json!("wrong-000002"),
                "run_id" => {
                    rows[1]["run_id"] = serde_json::json!("run-work-other");
                    rows[1]["event_id"] = serde_json::json!("run-work-other-000002");
                }
                "job" => rows[1]["job"] = serde_json::json!("arena"),
                "target" => rows[1]["target"]["repo"] = serde_json::json!("/other/repo"),
                _ => unreachable!(),
            }
            write_event_values(&path, &rows);

            assert!(
                read_events(&path).is_err(),
                "{corruption} corruption must fail closed"
            );
        }
    }

    #[test]
    fn run_event_read_rejects_malformed_hash_and_valid_json_without_newline() {
        let temp = TempDir::new("bad-hash");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let path = handle.events_path();
        let mut rows = event_values(&path);
        let original_rows = rows.clone();
        rows[0]["artifact_refs"][0]["sha256"] = serde_json::json!("not-a-sha256");
        write_event_values(&path, &rows);
        assert!(read_events(&path).is_err());

        write_event_values(&path, &original_rows);
        let mut bytes = std::fs::read(&path).expect("read events");
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, bytes).expect("remove final newline");
        assert!(read_events(&path).is_err());
    }

    #[test]
    fn run_event_manifest_rejects_malformed_hash() {
        let temp = TempDir::new("manifest-bad-hash");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let path = handle.manifest_path();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["artifacts"][0]["sha256"] = serde_json::json!("bad");
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        assert!(read_manifest(&path).is_err());
    }

    #[test]
    fn run_event_open_rejects_manifest_event_identity_mismatch() {
        let temp = TempDir::new("manifest-event-mismatch");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let run_id = handle.run_id().to_string();
        let manifest_path = handle.manifest_path();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["target"]["repo"] = serde_json::json!("/different/repo");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        assert!(RunHandle::open(temp.path(), &run_id).is_err());
    }

    #[test]
    fn run_event_cross_process_same_second_creation_is_exclusive() {
        const STATE_ENV: &str = "CONDUCTOR_RUN_TEST_CHILD_STATE";
        const RESULT_ENV: &str = "CONDUCTOR_RUN_TEST_CHILD_RESULT";
        if let (Some(state), Some(result)) =
            (std::env::var_os(STATE_ENV), std::env::var_os(RESULT_ENV))
        {
            let handle = RunHandle::create_at(
                Path::new(&state),
                RunJob::Work,
                new_run_request(),
                fixed_now(),
            )
            .expect("child creates run");
            std::fs::write(result, handle.run_id()).expect("child writes run id");
            return;
        }

        let temp = TempDir::new("cross-process");
        let current_exe = std::env::current_exe().expect("current test binary");
        let result_one = temp.path().join("child-one.id");
        let result_two = temp.path().join("child-two.id");
        let spawn = |result: &Path| {
            Command::new(&current_exe)
                .args([
                    "--exact",
                    "run::tests::run_event_cross_process_same_second_creation_is_exclusive",
                    "--nocapture",
                ])
                .env(STATE_ENV, temp.path())
                .env(RESULT_ENV, result)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child test")
        };
        let mut one = spawn(&result_one);
        let mut two = spawn(&result_two);
        assert!(one.wait().expect("wait child one").success());
        assert!(two.wait().expect("wait child two").success());

        let id_one = std::fs::read_to_string(result_one).expect("read child one id");
        let id_two = std::fs::read_to_string(result_two).expect("read child two id");
        assert_ne!(id_one, id_two);
        assert!(runs_dir(temp.path()).join(id_one).is_dir());
        assert!(runs_dir(temp.path()).join(id_two).is_dir());
    }

    fn event_values(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("read events")
            .lines()
            .map(|line| serde_json::from_str(line).expect("event JSON"))
            .collect()
    }

    fn write_event_values(path: &Path, rows: &[serde_json::Value]) {
        let mut content = rows
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        content.push('\n');
        std::fs::write(path, content).expect("write events");
    }
}
