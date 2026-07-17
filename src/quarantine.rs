//! Dirty-tree quarantine for failed or timed-out worker attempts.
//!
//! A worker can modify tracked files and create untracked files before
//! exiting nonzero, timing out, or losing provider capacity — all without
//! producing an accepted commit. Left alone, that leaves the real
//! repository dirty and strands both the partial work and every later
//! Conductor cycle targeting the same repo. This module captures that
//! uncommitted state as a hashed, immutable run artifact and restores the
//! repository to its exact pre-attempt state, but only when ownership can
//! be authenticated: HEAD must still match the value recorded before the
//! attempt started, and the post-restore state must itself be proven
//! clean. Any doubt fails closed and leaves the tree untouched.

#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};

use crate::dispatch::CommitProbe;
use crate::run::{ArtifactRef, RunHandle, RunJob, RunLifecycle, RunManifest};

pub(crate) type Result<T> = std::result::Result<T, QuarantineError>;

/// Bounds how many changed paths are recorded as run evidence. The
/// repository itself is never bounded — only the metadata durably recorded
/// about it, so a run with thousands of stray files cannot blow up the
/// event log.
const MAX_RECORDED_PATHS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuarantineError {
    /// HEAD moved since the attempt started (a real commit may be present).
    /// It is never safe to reset or clean in this state.
    HeadMoved {
        expected: Option<String>,
        found: Option<String>,
    },
    /// Repository state could not be read, or the dirty patch could not be
    /// captured. No destructive operation is attempted in this case.
    CaptureFailed(String),
    /// The patch was captured, but the restore step could not be proven to
    /// have returned the repository to a clean state at `before_head`. The
    /// captured evidence is not lost even when this happens.
    CleanupUnproven(String),
}

impl fmt::Display for QuarantineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadMoved { expected, found } => write!(
                f,
                "repository HEAD moved since the attempt started (expected {}, found {}); refusing to touch the working tree",
                expected.as_deref().unwrap_or("<none>"),
                found.as_deref().unwrap_or("<none>"),
            ),
            Self::CaptureFailed(message) => {
                write!(f, "failed to capture dirty repository state: {message}")
            }
            Self::CleanupUnproven(message) => {
                write!(f, "cleanup could not be proven complete: {message}")
            }
        }
    }
}

impl std::error::Error for QuarantineError {}

/// Outcome of one quarantine attempt: either the tree was already clean
/// (a no-op, so replaying quarantine on a clean tree is safe), or dirty
/// state was captured and the tree was restored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct QuarantineCapture {
    pub(crate) artifact: Option<ArtifactRef>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) truncated: bool,
}

impl QuarantineCapture {
    pub(crate) fn is_noop(&self) -> bool {
        self.artifact.is_none()
    }

    /// Bounded, human-readable summary safe to place in run event outcomes
    /// — counts and paths only, never patch content.
    pub(crate) fn summary(&self) -> String {
        let Some(artifact) = self.artifact.as_ref() else {
            return "clean".to_string();
        };
        format!(
            "quarantined {} path(s){} into {}#{}",
            self.changed_paths.len(),
            if self.truncated { " (truncated)" } else { "" },
            artifact.path,
            &artifact.sha256[..12],
        )
    }
}

/// Destructive repository mutation seam used only by quarantine recovery.
/// Kept separate from [`CommitProbe`] (a read-only seam many existing fakes
/// already implement) so these methods stay narrowly scoped to the one
/// place that is allowed to discard working-tree state.
pub(crate) trait RepoRecovery {
    /// Tracked and untracked (non-ignored) changed paths, repo-relative.
    fn changed_paths(&self, repo: &Path) -> std::result::Result<Vec<String>, String>;
    /// A single patch covering every tracked and untracked change.
    fn capture_patch(&self, repo: &Path) -> std::result::Result<Vec<u8>, String>;
    /// Discards tracked modifications and removes untracked (non-ignored)
    /// files, leaving the tree exactly at HEAD.
    fn restore_clean(&self, repo: &Path) -> std::result::Result<(), String>;
    /// Commit timestamp of HEAD. Used only to authenticate legacy runs
    /// recorded before `before_head` capture existed.
    fn head_committed_at(
        &self,
        repo: &Path,
    ) -> std::result::Result<Option<DateTime<Utc>>, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GitRepoRecovery;

impl RepoRecovery for GitRepoRecovery {
    fn changed_paths(&self, repo: &Path) -> std::result::Result<Vec<String>, String> {
        let status = git_text(repo, &["status", "--porcelain", "--untracked-files=normal"])?;
        Ok(status
            .lines()
            .filter_map(|line| line.get(3..).map(str::trim).map(str::to_string))
            .filter(|path| !path.is_empty())
            .collect())
    }

    fn capture_patch(&self, repo: &Path) -> std::result::Result<Vec<u8>, String> {
        let mut patch = git_bytes(repo, &["diff", "HEAD", "--"])?;
        let untracked = git_text(repo, &["ls-files", "--others", "--exclude-standard"])?;
        for path in untracked.lines().map(str::trim).filter(|p| !p.is_empty()) {
            let mut file_patch =
                git_bytes_diff_no_index(repo, &["diff", "--no-index", "--", "/dev/null", path])?;
            patch.append(&mut file_patch);
        }
        Ok(patch)
    }

    fn restore_clean(&self, repo: &Path) -> std::result::Result<(), String> {
        git_text(repo, &["reset", "--hard", "HEAD"])?;
        git_text(repo, &["clean", "-fd"])?;
        Ok(())
    }

    fn head_committed_at(
        &self,
        repo: &Path,
    ) -> std::result::Result<Option<DateTime<Utc>>, String> {
        let output = run_git(repo, &["log", "-1", "--format=%cI"])
            .map_err(|error| format!("failed to run git log in {}: {error}", repo.display()))?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            return Ok(None);
        }
        DateTime::parse_from_rfc3339(&text)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|error| format!("failed to parse commit timestamp {text:?}: {error}"))
    }
}

fn run_git(repo: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn git_text(repo: &Path, args: &[&str]) -> std::result::Result<String, String> {
    git_bytes(repo, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn git_bytes(repo: &Path, args: &[&str]) -> std::result::Result<Vec<u8>, String> {
    let output = run_git(repo, args)
        .map_err(|error| format!("failed to run git {} in {}: {error}", args.join(" "), repo.display()))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// `git diff --no-index` exits 1 when the compared paths differ — the
/// expected outcome for every untracked file — and only above 1 on a real
/// error, so exit code 1 is accepted here unlike every other git call.
fn git_bytes_diff_no_index(repo: &Path, args: &[&str]) -> std::result::Result<Vec<u8>, String> {
    let output = run_git(repo, args)
        .map_err(|error| format!("failed to run git {} in {}: {error}", args.join(" "), repo.display()))?;
    match output.status.code() {
        Some(0 | 1) => Ok(output.stdout),
        _ => Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Captures a worker's uncommitted changes (if any) as a hashed run
/// artifact and restores the repository to `before_head`.
///
/// Returns `Ok(QuarantineCapture::default())` — a no-op — when the tree is
/// already clean, so calling this repeatedly on an already-restored tree is
/// safe. Never mutates the tree unless HEAD still matches `before_head`;
/// never reports success unless the post-restore tree is proven clean and
/// still at `before_head`.
pub(crate) fn quarantine_dirty_attempt<C, R>(
    repo: &Path,
    commits: &C,
    recovery: &R,
    run_artifacts: &RunHandle,
    before_head: Option<&str>,
    artifact_label: &str,
) -> Result<QuarantineCapture>
where
    C: CommitProbe + ?Sized,
    R: RepoRecovery + ?Sized,
{
    let current_head = commits
        .head(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git head: {error}")))?;
    if current_head.as_deref() != before_head {
        return Err(QuarantineError::HeadMoved {
            expected: before_head.map(str::to_string),
            found: current_head,
        });
    }
    let is_clean = commits
        .is_clean(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git status: {error}")))?;
    if is_clean {
        return Ok(QuarantineCapture::default());
    }

    let mut changed_paths = recovery
        .changed_paths(repo)
        .map_err(QuarantineError::CaptureFailed)?;
    let truncated = changed_paths.len() > MAX_RECORDED_PATHS;
    changed_paths.truncate(MAX_RECORDED_PATHS);

    let patch = recovery
        .capture_patch(repo)
        .map_err(QuarantineError::CaptureFailed)?;
    if patch.is_empty() {
        return Err(QuarantineError::CaptureFailed(
            "repository is dirty but no patch content could be captured".to_string(),
        ));
    }
    let artifact = write_patch_artifact(run_artifacts, artifact_label, &patch)
        .map_err(QuarantineError::CaptureFailed)?;

    recovery
        .restore_clean(repo)
        .map_err(QuarantineError::CleanupUnproven)?;

    let post_head = commits.head(repo).map_err(|error| {
        QuarantineError::CleanupUnproven(format!("git head after restore: {error}"))
    })?;
    if post_head.as_deref() != before_head {
        return Err(QuarantineError::CleanupUnproven(format!(
            "HEAD changed during restore: expected {}, found {}",
            before_head.unwrap_or("<none>"),
            post_head.as_deref().unwrap_or("<none>"),
        )));
    }
    let post_clean = commits.is_clean(repo).map_err(|error| {
        QuarantineError::CleanupUnproven(format!("git status after restore: {error}"))
    })?;
    if !post_clean {
        return Err(QuarantineError::CleanupUnproven(
            "repository is still dirty after restore".to_string(),
        ));
    }

    Ok(QuarantineCapture {
        artifact: Some(artifact),
        changed_paths,
        truncated,
    })
}

fn write_patch_artifact(
    run_artifacts: &RunHandle,
    artifact_label: &str,
    patch: &[u8],
) -> std::result::Result<ArtifactRef, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!(
        "conductor-quarantine-{}-{}-{nanos}.patch",
        std::process::id(),
        sanitize(artifact_label),
    ));
    std::fs::write(&tmp, patch)
        .map_err(|error| format!("failed to stage quarantine patch: {error}"))?;
    let destination = PathBuf::from("artifacts").join(format!("{}.patch", sanitize(artifact_label)));
    let result = run_artifacts
        .capture_artifact(&tmp, &destination)
        .map_err(|error| format!("failed to capture quarantine artifact: {}", error.into_message()));
    let _ = std::fs::remove_file(&tmp);
    result
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// A prior Finished work run for the same repo/bead whose outcome was not a
/// verified pass — the only kind of run whose dirty leftovers are eligible
/// for one-time legacy adoption ahead of a fresh dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdoptableRun {
    pub(crate) run_id: String,
    pub(crate) before_head: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

/// Scans every run recorded under `state_dir` and returns the single
/// most-recently-created Finished work run targeting `repo`/`bead`, if that
/// run's outcome was not a verified pass. Returns `Err` — never a silent
/// `Ok(None)` — when a run manifest that targets this repo/bead cannot be
/// read cleanly, since a tampered or corrupt manifest for our own target is
/// exactly the ambiguous-provenance case recovery must refuse. Manifests
/// for other targets are ignored regardless of their own validity.
pub(crate) fn most_recent_failed_run(
    state_dir: &Path,
    repo: &str,
    bead: &str,
) -> Result<Option<AdoptableRun>> {
    let root = crate::run::runs_dir(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(QuarantineError::CaptureFailed(format!(
                "failed to read runs dir {}: {error}",
                root.display()
            )));
        }
    };
    let mut latest: Option<(DateTime<Utc>, RunManifest)> = None;
    for entry in entries {
        let entry = entry.map_err(|error| {
            QuarantineError::CaptureFailed(format!("failed to read run directory entry: {error}"))
        })?;
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        if !manifest_targets(&manifest_path, repo, bead) {
            continue;
        }
        // This manifest claims our exact target: it must now validate
        // strictly, or we cannot trust any run evidence for this target.
        let manifest = crate::run::read_manifest(&manifest_path).map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "run evidence for {repo}/{bead} at {} failed validation: {}",
                manifest_path.display(),
                error.into_message()
            ))
        })?;
        if manifest.job != RunJob::Work || manifest.lifecycle != RunLifecycle::Finished {
            continue;
        }
        let Ok(created_at) = DateTime::parse_from_rfc3339(&manifest.created_at) else {
            return Err(QuarantineError::CaptureFailed(format!(
                "run evidence for {repo}/{bead} has an unparseable created_at timestamp"
            )));
        };
        let created_at = created_at.with_timezone(&Utc);
        let is_more_recent = match latest.as_ref() {
            Some((current, _)) => created_at > *current,
            None => true,
        };
        if is_more_recent {
            latest = Some((created_at, manifest));
        }
    }
    let Some((created_at, manifest)) = latest else {
        return Ok(None);
    };
    if manifest.outcome.as_deref() == Some("verified") {
        return Ok(None);
    }
    let before_head = manifest.work.and_then(|work| work.before_head);
    Ok(Some(AdoptableRun {
        run_id: manifest.run_id,
        before_head,
        created_at,
    }))
}

/// Cheap, non-strict peek at whether a manifest targets `repo`/`bead`,
/// without requiring it to pass full schema/hash validation first — used
/// only to decide whether a broken manifest belongs to our own recovery
/// decision (and must therefore block it) or to an unrelated run (and can
/// be safely ignored).
fn manifest_targets(manifest_path: &Path, repo: &str, bead: &str) -> bool {
    let Ok(bytes) = std::fs::read(manifest_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("job").and_then(serde_json::Value::as_str) == Some("work")
        && value
            .get("target")
            .and_then(|target| target.get("repo"))
            .and_then(serde_json::Value::as_str)
            == Some(repo)
        && value
            .get("target")
            .and_then(|target| target.get("bead"))
            .and_then(serde_json::Value::as_str)
            == Some(bead)
}

/// Authenticates that no commit has landed on `repo` since `run` failed,
/// so its dirty leftovers can be safely attributed to that run alone.
/// Prefers an exact HEAD match against `run.before_head` when the manifest
/// recorded one; falls back to comparing HEAD's own commit timestamp
/// against when the run started for manifests written before that field
/// existed. Never mutates the repository.
pub(crate) fn authenticate_legacy_adoption<C, R>(
    commits: &C,
    recovery: &R,
    repo: &Path,
    run: &AdoptableRun,
) -> Result<String>
where
    C: CommitProbe + ?Sized,
    R: RepoRecovery + ?Sized,
{
    let current_head = commits
        .head(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git head: {error}")))?;
    if let Some(expected) = run.before_head.as_deref() {
        if current_head.as_deref() != Some(expected) {
            return Err(QuarantineError::HeadMoved {
                expected: Some(expected.to_string()),
                found: current_head,
            });
        }
        return Ok(expected.to_string());
    }
    let Some(current_head) = current_head else {
        return Err(QuarantineError::CaptureFailed(
            "repository has no HEAD to authenticate against".to_string(),
        ));
    };
    let committed_at = recovery
        .head_committed_at(repo)
        .map_err(QuarantineError::CaptureFailed)?
        .ok_or_else(|| {
            QuarantineError::CaptureFailed(
                "could not determine HEAD commit timestamp to authenticate legacy recovery"
                    .to_string(),
            )
        })?;
    if committed_at > run.created_at {
        return Err(QuarantineError::HeadMoved {
            expected: None,
            found: Some(current_head.clone()),
        });
    }
    Ok(current_head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-quarantine-{label}-{nanos}"));
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

    /// Sequential fakes: each call pops the next programmed result, mirroring
    /// `dispatch::tests::FakeCommits`. `heads`/`cleans` are consumed in the
    /// exact order `quarantine_dirty_attempt` calls them: HEAD (preflight),
    /// `is_clean` (preflight), then — only if dirty and restore is attempted
    /// — HEAD (post-restore), `is_clean` (post-restore).
    struct FakeCommits {
        heads: RefCell<Vec<Option<String>>>,
        cleans: RefCell<Vec<bool>>,
    }

    impl FakeCommits {
        fn new<const N: usize, const M: usize>(heads: [Option<&str>; N], cleans: [bool; M]) -> Self {
            Self {
                heads: RefCell::new(heads.into_iter().map(|h| h.map(str::to_string)).collect()),
                cleans: RefCell::new(cleans.to_vec()),
            }
        }
    }

    impl CommitProbe for FakeCommits {
        fn head(&self, _repo: &Path) -> crate::dispatch::Result<Option<String>> {
            Ok(self.heads.borrow_mut().remove(0))
        }

        fn is_clean(&self, _repo: &Path) -> crate::dispatch::Result<bool> {
            Ok(self.cleans.borrow_mut().remove(0))
        }
    }

    #[derive(Default)]
    struct FakeRecovery {
        changed_paths: Vec<String>,
        patch: Vec<u8>,
        restore_error: RefCell<Option<String>>,
        restore_calls: RefCell<u32>,
        head_committed_at: Option<DateTime<Utc>>,
    }

    impl RepoRecovery for FakeRecovery {
        fn changed_paths(&self, _repo: &Path) -> std::result::Result<Vec<String>, String> {
            Ok(self.changed_paths.clone())
        }

        fn capture_patch(&self, _repo: &Path) -> std::result::Result<Vec<u8>, String> {
            Ok(self.patch.clone())
        }

        fn restore_clean(&self, _repo: &Path) -> std::result::Result<(), String> {
            *self.restore_calls.borrow_mut() += 1;
            match self.restore_error.borrow().clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }

        fn head_committed_at(
            &self,
            _repo: &Path,
        ) -> std::result::Result<Option<DateTime<Utc>>, String> {
            Ok(self.head_committed_at)
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        "2026-07-17T14:45:26Z".parse().expect("fixed timestamp")
    }

    fn run_handle(temp: &TempDir, label: &str) -> RunHandle {
        RunHandle::create(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: "/repo/conductor".to_string(),
                    bead: Some(format!("bead-{label}")),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-1".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: Some("b".repeat(40)),
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
        )
        .expect("create run")
    }

    #[test]
    fn quarantine_worker_failure_captures_tracked_edits_and_restores_repo() {
        let temp = TempDir::new("tracked");
        let handle = run_handle(&temp, "tracked");
        let commits = FakeCommits::new(
            [Some("head1"), Some("head1")],
            [false, true],
        );
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            ..FakeRecovery::default()
        };

        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "001-attempt",
        )
        .expect("capture succeeds");

        assert!(!capture.is_noop());
        assert_eq!(capture.changed_paths, vec!["src/lib.rs".to_string()]);
        assert!(!capture.truncated);
        let artifact = capture.artifact.expect("artifact captured");
        assert_eq!(artifact.path, "artifacts/001-attempt.patch");
        assert_eq!(*recovery.restore_calls.borrow(), 1);
        assert_eq!(
            std::fs::read(handle.dir().join(&artifact.path)).expect("read captured patch"),
            recovery.patch,
        );
    }

    #[test]
    fn quarantine_worker_failure_captures_untracked_files() {
        let temp = TempDir::new("untracked");
        let handle = run_handle(&temp, "untracked");
        let commits = FakeCommits::new(
            [Some("head1"), Some("head1")],
            [false, true],
        );
        let recovery = FakeRecovery {
            changed_paths: vec!["fixtures/new_case.json".to_string()],
            patch: b"diff --git a/fixtures/new_case.json b/fixtures/new_case.json\nnew file\n"
                .to_vec(),
            ..FakeRecovery::default()
        };

        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "002-attempt",
        )
        .expect("capture succeeds");

        assert_eq!(
            capture.changed_paths,
            vec!["fixtures/new_case.json".to_string()]
        );
        assert!(capture.artifact.is_some());
    }

    #[test]
    fn quarantine_replay_on_already_clean_tree_is_idempotent_noop() {
        let temp = TempDir::new("idempotent");
        let handle = run_handle(&temp, "idempotent");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [true, true]);
        let recovery = FakeRecovery::default();

        let first = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "003-attempt",
        )
        .expect("first call succeeds");
        let second = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "003-attempt",
        )
        .expect("second call succeeds");

        assert!(first.is_noop());
        assert!(second.is_noop());
        assert_eq!(*recovery.restore_calls.borrow(), 0);
    }

    #[test]
    fn quarantine_head_moved_since_attempt_start_fails_closed_untouched() {
        let temp = TempDir::new("head-moved");
        let handle = run_handle(&temp, "head-moved");
        let commits = FakeCommits::new([Some("head2")], []);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff\n".to_vec(),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "004-attempt",
        )
        .expect_err("head moved must fail closed");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
        assert_eq!(*recovery.restore_calls.borrow(), 0, "must not mutate tree");
    }

    #[test]
    fn quarantine_committed_then_failed_is_treated_as_head_moved() {
        // A worker that commits and then fails later (e.g. a post-commit
        // crash) must never be treated as an uncommitted dirty attempt.
        let temp = TempDir::new("committed-then-failed");
        let handle = run_handle(&temp, "committed-then-failed");
        let commits = FakeCommits::new([Some("committed-sha")], []);
        let recovery = FakeRecovery::default();

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("pre-attempt-sha"),
            "005-attempt",
        )
        .expect_err("committed-then-failed must fail closed");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
        assert_eq!(*recovery.restore_calls.borrow(), 0);
    }

    #[test]
    fn quarantine_cleanup_failure_reports_diagnostic_without_losing_capture() {
        let temp = TempDir::new("cleanup-failure");
        let handle = run_handle(&temp, "cleanup-failure");
        let commits = FakeCommits::new([Some("head1")], [false]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            restore_error: RefCell::new(Some("permission denied removing stray.tmp".to_string())),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "006-attempt",
        )
        .expect_err("cleanup failure must fail closed");

        assert!(matches!(error, QuarantineError::CleanupUnproven(_)));
        // The patch is still durably captured on disk even though the
        // restore step itself failed — no data loss on partial failure.
        assert!(handle.dir().join("artifacts/006-attempt.patch").is_file());
    }

    #[test]
    fn quarantine_cleanup_that_leaves_tree_dirty_is_not_proven_and_fails_closed() {
        let temp = TempDir::new("cleanup-unproven");
        let handle = run_handle(&temp, "cleanup-unproven");
        let commits = FakeCommits::new(
            [Some("head1"), Some("head1")],
            [false, false],
        );
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff\n".to_vec(),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "007-attempt",
        )
        .expect_err("unproven cleanup must fail closed");

        assert!(matches!(error, QuarantineError::CleanupUnproven(_)));
    }

    #[test]
    fn quarantine_captured_artifact_tamper_is_detected_by_hash_validation() {
        let temp = TempDir::new("tamper");
        let handle = run_handle(&temp, "tamper");
        let commits = FakeCommits::new(
            [Some("head1"), Some("head1")],
            [false, true],
        );
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            ..FakeRecovery::default()
        };
        let run_id = handle.run_id().to_string();
        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "008-attempt",
        )
        .expect("capture succeeds");
        drop(handle);
        let artifact = capture.artifact.expect("artifact captured");

        std::fs::write(
            temp.path().join("runs").join(&run_id).join(&artifact.path),
            b"tampered",
        )
        .expect("tamper artifact bytes");

        // The tampered bytes no longer match the hash recorded at capture
        // time — the same tamper-evidence property `run.rs` already proves
        // for every other artifact captured through `RunHandle`.
        assert_ne!(format!("{:x}", Sha256::digest(b"tampered")), artifact.sha256);
        assert!(
            RunHandle::open(temp.path(), &run_id).is_ok(),
            "manifest itself is untouched by tampering an artifact it never referenced"
        );
    }

    #[test]
    fn quarantine_no_raw_patch_bytes_ever_reach_the_manifest_or_event_log() {
        let temp = TempDir::new("no-raw-patch");
        let handle = run_handle(&temp, "no-raw-patch");
        let commits = FakeCommits::new(
            [Some("head1"), Some("head1")],
            [false, true],
        );
        let secret_marker = "TOTALLY_UNIQUE_DIRTY_PATCH_CONTENT_MARKER";
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: format!("diff --git a/src/lib.rs b/src/lib.rs\n+{secret_marker}\n")
                .into_bytes(),
            ..FakeRecovery::default()
        };
        let run_id = handle.run_id().to_string();

        quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "009-attempt",
        )
        .expect("capture succeeds");
        drop(handle);

        let manifest_text = std::fs::read_to_string(
            temp.path().join("runs").join(&run_id).join("manifest.json"),
        )
        .expect("read manifest");
        let events_text = std::fs::read_to_string(
            temp.path().join("runs").join(&run_id).join("events.jsonl"),
        )
        .expect("read events");
        assert!(!manifest_text.contains(secret_marker));
        assert!(!events_text.contains(secret_marker));
    }

    fn manifest_for(
        temp: &TempDir,
        repo: &str,
        bead: &str,
        outcome: &str,
        before_head: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> String {
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: repo.to_string(),
                    bead: Some(bead.to_string()),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-legacy".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: before_head.map(str::to_string),
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
            created_at,
        )
        .expect("create legacy run");
        handle.finish(outcome).expect("finish legacy run");
        handle.run_id().to_string()
    }

    #[test]
    fn most_recent_failed_run_finds_unique_recent_failure_for_target() {
        let temp = TempDir::new("legacy-lookup");
        let created_at = fixed_now();
        let run_id = manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            created_at,
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds")
            .expect("run found");

        assert_eq!(found.run_id, run_id);
        assert_eq!(found.before_head, None);
        assert_eq!(found.created_at, created_at);
    }

    #[test]
    fn most_recent_failed_run_ignores_runs_for_other_targets() {
        let temp = TempDir::new("legacy-other-target");
        manifest_for(
            &temp,
            "/repo/other",
            "other-1",
            "failed",
            None,
            fixed_now(),
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds");

        assert_eq!(found, None);
    }

    #[test]
    fn most_recent_failed_run_refuses_when_a_later_run_supersedes_the_failure() {
        let temp = TempDir::new("legacy-superseded");
        manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "verified",
            None,
            fixed_now() + chrono::Duration::minutes(5),
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds");

        assert_eq!(found, None, "a later verified run means nothing to adopt");
    }

    #[test]
    fn most_recent_failed_run_fails_closed_on_tampered_evidence_for_our_target() {
        let temp = TempDir::new("legacy-tampered");
        let run_id = manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        let manifest_path = temp.path().join("runs").join(&run_id).join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["work"]["authorization_sha256"] = serde_json::json!("not-a-sha256");
        std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        let error = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect_err("tampered evidence for our own target must fail closed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn authenticate_legacy_adoption_matches_recorded_before_head_exactly() {
        let commits = FakeCommits::new([Some("sha-a")], []);
        let recovery = FakeRecovery::default();
        let run = AdoptableRun {
            run_id: "run-work-1".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
        };

        let head =
            authenticate_legacy_adoption(&commits, &recovery, Path::new("/repo/bursar"), &run)
                .expect("authentication succeeds");

        assert_eq!(head, "sha-a");
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_when_recorded_head_does_not_match() {
        let commits = FakeCommits::new([Some("sha-b")], []);
        let recovery = FakeRecovery::default();
        let run = AdoptableRun {
            run_id: "run-work-1".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
        };

        let error =
            authenticate_legacy_adoption(&commits, &recovery, Path::new("/repo/bursar"), &run)
                .expect_err("head mismatch must refuse");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
    }

    #[test]
    fn authenticate_legacy_adoption_falls_back_to_commit_timestamp_for_manifests_without_before_head()
     {
        let commits = FakeCommits::new([Some("sha-legacy")], []);
        let recovery = FakeRecovery {
            head_committed_at: Some(fixed_now() - chrono::Duration::minutes(10)),
            ..FakeRecovery::default()
        };
        let run = AdoptableRun {
            run_id: "run-work-legacy".to_string(),
            before_head: None,
            created_at: fixed_now(),
        };

        let head =
            authenticate_legacy_adoption(&commits, &recovery, Path::new("/repo/bursar"), &run)
                .expect("timestamp heuristic authenticates");

        assert_eq!(head, "sha-legacy");
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_when_head_commit_postdates_the_failed_run() {
        let commits = FakeCommits::new([Some("sha-newer")], []);
        let recovery = FakeRecovery {
            head_committed_at: Some(fixed_now() + chrono::Duration::minutes(1)),
            ..FakeRecovery::default()
        };
        let run = AdoptableRun {
            run_id: "run-work-legacy".to_string(),
            before_head: None,
            created_at: fixed_now(),
        };

        let error =
            authenticate_legacy_adoption(&commits, &recovery, Path::new("/repo/bursar"), &run)
                .expect_err("a newer commit must refuse adoption");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
    }
}
