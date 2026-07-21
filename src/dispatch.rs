//! backend runners (pi/agy/claude/codex) behind a trait (Exec) + timeout/kill

// Built ahead of the M4 integration path; unit tests exercise this module directly.
#![allow(dead_code)]

use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Write as _};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Backend, ReasoningEffort};

const PI_THINKING: &str = "xhigh";
const ATTEMPT_IDENTITY_NAME: &str = "Conductor Worker Attempt";
const KILL_GRACE: Duration = Duration::from_secs(3);
const WAIT_POLL: Duration = Duration::from_millis(50);

pub(crate) type Result<T> = std::result::Result<T, DispatchError>;

#[derive(Debug, Clone)]
pub(crate) struct DispatchError {
    message: String,
    worker_state_uncertain: bool,
}

impl DispatchError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            worker_state_uncertain: false,
        }
    }

    fn worker_state_uncertain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            worker_state_uncertain: true,
        }
    }

    pub(crate) const fn leaves_worker_state_uncertain(&self) -> bool {
        self.worker_state_uncertain
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DispatchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchRequest {
    pub(crate) repo: PathBuf,
    pub(crate) before_head: Option<String>,
    pub(crate) attempt_id: String,
    pub(crate) cycle_id: String,
    pub(crate) bead_id: String,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) prompt: String,
    /// A per-attempt audit identity. Real worker authority comes from the
    /// kernel-authenticated commit receipt, not this observable value.
    pub(crate) attempt_identity: String,
    /// Parent-authored Seatbelt profile which confines this worker and all of
    /// its descendants to the current isolated checkout.
    pub(crate) sandbox_profile: Option<PathBuf>,
}

/// Mints a unique audit identity for a single worker attempt's Git metadata.
///
/// This value is intentionally not authority: another same-UID process can
/// observe or recreate environment and commit metadata. Real worker authority
/// is the kernel-authenticated socket receipt handled by [`CommandChild`].
pub(crate) fn attempt_commit_identity() -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::hash::BuildHasher;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static MINTED: AtomicU64 = AtomicU64::new(0);

    let mut hasher = Sha256::new();
    let mut seed = [0u8; 32];
    if File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut seed))
        .is_ok()
    {
        hasher.update(seed);
    }
    // Mixed in unconditionally so a platform without `/dev/urandom` still
    // yields a value an already-running process cannot predict.
    let per_process = std::collections::hash_map::RandomState::new();
    hasher.update(
        per_process
            .hash_one(MINTED.fetch_add(1, Ordering::Relaxed))
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    if let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) {
        hasher.update(since_epoch.as_nanos().to_le_bytes());
    }
    let mut nonce = String::with_capacity(32);
    for byte in hasher.finalize().iter().take(16) {
        let _ = write!(nonce, "{byte:02x}");
    }
    format!("conductor-attempt-{nonce}@invalid")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchResult {
    pub(crate) status: DispatchStatus,
    pub(crate) worker_commit: Option<String>,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchStatus {
    Success,
    Failed(DispatchFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchFailure {
    TimedOut,
    ExitNonZero { code: Option<i32> },
    NoNewCommit,
    UnauthenticatedCommit,
    BackendFlakeZeroStdoutNoCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) stdin: StdinMode,
    pub(crate) sandbox_profile: Option<PathBuf>,
    pub(crate) commit_receipt_socket: Option<PathBuf>,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StdinMode {
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessStatus {
    code: Option<i32>,
    success: bool,
}

impl ProcessStatus {
    pub(crate) const fn code(code: i32) -> Self {
        Self {
            code: Some(code),
            success: code == 0,
        }
    }

    pub(crate) const fn signal() -> Self {
        Self {
            code: None,
            success: false,
        }
    }

    pub(crate) const fn exit_code(self) -> Option<i32> {
        self.code
    }

    pub(crate) const fn success(self) -> bool {
        self.success
    }
}

impl From<ExitStatus> for ProcessStatus {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

pub(crate) trait Exec {
    fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>>;
}

pub(crate) trait ChildProcess {
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>>;
    fn terminate(&mut self) -> Result<()>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<ProcessStatus>;
    /// The child's OS pid, if it is a real process. Because workers are
    /// spawned as the leader of their own process group (see
    /// [`set_own_process_group`]), this pid also names that group — the durable
    /// identity stale-claim recovery binds to via
    /// [`WorkerHooks::on_spawn`]. In-memory test doubles return `None`, which
    /// recovery treats as an unprovable worker identity and fails closed on.
    fn id(&self) -> Option<u32> {
        None
    }
    /// Returns the only commit hash reported by a post-commit hook whose
    /// connecting process the kernel proved descends from this worker root.
    /// In-memory test doubles have no OS lineage and return `None`.
    fn authenticated_worker_commit(&self) -> Option<String> {
        None
    }
    /// Proves that the worker's process group has no surviving descendants
    /// after the direct child exits. A real worker may fork background tools
    /// that outlive the harness process; those descendants must be terminated
    /// before an attempt checkout is removed or a fallback checkout is
    /// created. Test doubles without an OS pid have no recorded group to
    /// prove.
    fn ensure_process_group_quiescent(&mut self) -> Result<()> {
        self.id().map_or(Ok(()), ensure_process_group_quiescent)
    }
    /// Proves process-group quiescence. A descendant which changes session is
    /// contained by the irreversible per-attempt filesystem sandbox instead
    /// of an inherited descriptor.
    fn ensure_worker_quiescent(&mut self) -> Result<()> {
        self.ensure_process_group_quiescent()
    }
}

/// Callbacks the worker runtime invokes around a dispatched worker's lifetime.
/// A single observer (rather than separate closures) so it can hold one
/// exclusive borrow of the run's durable state across both the one-shot
/// spawn hooks and the repeated heartbeat ticks.
pub(crate) trait WorkerHooks {
    /// Invoked exactly once, immediately before the worker is spawned, to
    /// durably invalidate any earlier attempt's recorded process-group
    /// identity. Only after this returns `Ok` does [`run_with_heartbeat`]
    /// spawn the new worker, so a crash between this call and the matching
    /// [`WorkerHooks::on_spawn`] leaves the run's durable state holding no
    /// worker identity at all — never a superseded attempt's (by-then-dead)
    /// group, which recovery could otherwise mistake for proof this new,
    /// still-unrecorded attempt died too. Returning `Err` prevents the spawn
    /// entirely: nothing has started yet, so there is nothing to reap.
    fn on_pre_spawn(&mut self) -> Result<()> {
        Ok(())
    }
    /// Invoked exactly once, immediately after the worker is spawned and
    /// before it can meaningfully mutate the repository, with the worker's pid
    /// (which also names its process group). Returning an `Err` fails closed:
    /// the just-spawned worker is terminated and reaped before this error
    /// propagates, so a worker whose identity could not be durably recorded
    /// never runs unattended.
    fn on_spawn(&mut self, _pid: Option<u32>) -> Result<()> {
        Ok(())
    }
    /// Invoked on each heartbeat tick while the worker runs.
    fn on_heartbeat(&mut self, _elapsed: Duration) -> Result<()> {
        Ok(())
    }
}

/// No-op hooks for callers that dispatch a process needing neither durable
/// worker-group binding nor heartbeats (e.g. the plain [`run`] wrapper and
/// read-only reviewer probes).
impl WorkerHooks for () {}

pub(crate) trait CommitProbe {
    fn head(&self, repo: &Path) -> Result<Option<String>>;
    fn is_clean(&self, repo: &Path) -> Result<bool>;
    /// Proves `commit` is the single commit immediately after `before`.
    /// Dispatch only invokes this against a parent-created, attempt-specific
    /// checkout, so the observed checkout HEAD — never worker-controlled
    /// stdout — is what dispatch reads the commit from.
    fn is_direct_child(&self, repo: &Path, before: Option<&str>, commit: &str) -> Result<bool>;
    /// The committer email recorded on `commit`. Production workers never use
    /// this observable metadata as authority; it remains for audit evidence
    /// and in-memory tests which cannot model an OS process lineage.
    fn committer_email(&self, repo: &Path, commit: &str) -> Result<Option<String>>;
}

pub(crate) fn run<E: Exec, C: CommitProbe>(
    exec: &E,
    commits: &C,
    request: &DispatchRequest,
    state_dir: &Path,
    timeout: Duration,
) -> Result<DispatchResult> {
    run_with_heartbeat(exec, commits, request, state_dir, timeout, timeout, &mut ())
}

pub(crate) fn run_readonly<E: Exec + ?Sized>(
    exec: &E,
    request: &SpawnRequest,
    timeout: Duration,
) -> Result<()> {
    let mut child = exec.spawn(request)?;
    let process = wait_with_timeout_and_heartbeat(child.as_mut(), timeout, timeout, &mut ())?;
    if process.timed_out {
        return Err(DispatchError::new("read-only process timed out"));
    }
    if process.status.success {
        Ok(())
    } else {
        Err(DispatchError::new(format!(
            "read-only process exited with status {}",
            process
                .status
                .code
                .map_or_else(|| "signal".to_string(), |code| code.to_string())
        )))
    }
}

pub(crate) fn run_with_heartbeat<E, C, K>(
    exec: &E,
    commits: &C,
    request: &DispatchRequest,
    state_dir: &Path,
    timeout: Duration,
    heartbeat_interval: Duration,
    hooks: &mut K,
) -> Result<DispatchResult>
where
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
    K: WorkerHooks + ?Sized,
{
    let spawn = spawn_request(request, state_dir)?;
    let attempt_head = commits.head(&request.repo)?;
    if attempt_head != request.before_head {
        return Ok(DispatchResult {
            status: DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
            worker_commit: None,
            stdout_path: spawn.stdout_path,
            stderr_path: spawn.stderr_path,
            stdout_bytes: 0,
            stderr_bytes: 0,
        });
    }
    // Durably invalidate any earlier attempt's worker-group identity before
    // this attempt's process exists at all. A failure here must prevent the
    // spawn outright — see `WorkerHooks::on_pre_spawn`.
    hooks.on_pre_spawn()?;
    let mut child = exec.spawn(&spawn)?;
    #[cfg(test)]
    let requires_kernel_authentication = child.id().is_some();
    #[cfg(not(test))]
    let requires_kernel_authentication = true;
    // Bind the run to this worker's process group before it can meaningfully
    // mutate the repository. If that durable record fails, tear the worker
    // (and any descendants) down rather than let a worker whose identity we
    // cannot prove keep running unattended.
    if let Err(error) = hooks.on_spawn(child.id()) {
        if terminate_and_reap_best_effort(child.as_mut()) {
            return Err(error);
        }
        return Err(DispatchError::worker_state_uncertain(format!(
            "{error}; spawned worker process group could not be proven quiescent"
        )));
    }
    let process =
        wait_with_timeout_and_heartbeat(child.as_mut(), timeout, heartbeat_interval, hooks)?;
    let authenticated_commit = child.authenticated_worker_commit();
    let stdout_bytes = file_len(&spawn.stdout_path)?;
    let stderr_bytes = file_len(&spawn.stderr_path)?;
    let authentication = CommitAuthentication {
        attempt_identity: &request.attempt_identity,
        authenticated_commit: authenticated_commit.as_deref(),
        requires_kernel_authentication,
    };
    let (status, worker_commit) = classify(
        process,
        stdout_bytes,
        request.before_head.as_deref(),
        commits,
        &request.repo,
        authentication,
    )?;

    Ok(DispatchResult {
        status,
        worker_commit,
        stdout_path: spawn.stdout_path,
        stderr_path: spawn.stderr_path,
        stdout_bytes,
        stderr_bytes,
    })
}

fn spawn_request(request: &DispatchRequest, state_dir: &Path) -> Result<SpawnRequest> {
    let log_dir = state_dir.join("logs").join(&request.cycle_id);
    fs::create_dir_all(&log_dir).map_err(|e| {
        DispatchError::new(format!(
            "failed to create log dir {}: {e}",
            log_dir.display()
        ))
    })?;
    let attempt_log_dir = log_dir.join(&request.bead_id);
    fs::create_dir_all(&attempt_log_dir).map_err(|e| {
        DispatchError::new(format!(
            "failed to create attempt log dir {}: {e}",
            attempt_log_dir.display()
        ))
    })?;
    let stdout_path = attempt_log_dir.join(format!("{}.out", request.attempt_id));
    let stderr_path = attempt_log_dir.join(format!("{}.err", request.attempt_id));
    File::create(&stdout_path).map_err(|e| {
        DispatchError::new(format!(
            "failed to create stdout log {}: {e}",
            stdout_path.display()
        ))
    })?;
    File::create(&stderr_path).map_err(|e| {
        DispatchError::new(format!(
            "failed to create stderr log {}: {e}",
            stderr_path.display()
        ))
    })?;

    let mut argv = argv_for_backend(
        request.backend,
        &request.dispatch_id,
        request.reasoning_effort,
        &request.prompt,
        &request.repo,
    )?;
    // The outer Seatbelt profile is the single filesystem authority for a
    // worker. Disable a harness's nested sandbox only when that parent-owned
    // profile is actually present; a missing profile must fail safe.
    if request.sandbox_profile.is_some() {
        match request.backend {
            Backend::Codex => {
                argv.insert(2, "--dangerously-bypass-approvals-and-sandbox".to_string());
            }
            Backend::Claude => argv.push("--dangerously-skip-permissions".to_string()),
            Backend::Pi | Backend::Omp | Backend::Agy => {}
        }
    }
    let receipt_socket = commit_receipt_socket_path(&request.attempt_identity);
    let hook_dir = prepare_authenticated_commit_hook(state_dir, &request.attempt_identity)?;

    Ok(SpawnRequest {
        argv,
        cwd: request.repo.clone(),
        // Git identity remains useful audit evidence. The socket receipt is
        // the authority: its peer pid is authenticated by the kernel.
        env: vec![
            (
                "GIT_AUTHOR_NAME".to_string(),
                ATTEMPT_IDENTITY_NAME.to_string(),
            ),
            (
                "GIT_AUTHOR_EMAIL".to_string(),
                request.attempt_identity.clone(),
            ),
            (
                "GIT_COMMITTER_NAME".to_string(),
                ATTEMPT_IDENTITY_NAME.to_string(),
            ),
            (
                "GIT_COMMITTER_EMAIL".to_string(),
                request.attempt_identity.clone(),
            ),
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "core.hooksPath".to_string()),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                hook_dir.display().to_string(),
            ),
            (
                "CONDUCTOR_COMMIT_RECEIPT_SOCKET".to_string(),
                receipt_socket.display().to_string(),
            ),
        ],
        stdin: StdinMode::Null,
        sandbox_profile: request.sandbox_profile.clone(),
        commit_receipt_socket: Some(receipt_socket),
        stdout_path,
        stderr_path,
    })
}

pub(crate) fn argv_for_backend(
    backend: Backend,
    dispatch_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    prompt: &str,
    repo: &Path,
) -> Result<Vec<String>> {
    Ok(match backend {
        Backend::Pi => strings([
            "pi",
            "--model",
            dispatch_id,
            "--thinking",
            PI_THINKING,
            "--approve",
            "-p",
            prompt,
        ]),
        Backend::Omp => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("OMP dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "omp".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--thinking".to_string(),
                effort.as_str().to_string(),
                "--auto-approve".to_string(),
                "--no-session".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Codex => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("Codex dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "codex".to_string(),
                "exec".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--config".to_string(),
                format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                prompt.to_string(),
            ]
        }
        Backend::Agy => vec![
            "agy".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--add-dir".to_string(),
            repo.display().to_string(),
            "--model".to_string(),
            dispatch_id.to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
        Backend::Claude => strings(["claude", "-p", prompt, "--model", dispatch_id]),
    })
}

pub(crate) fn readonly_argv_for_backend(
    backend: Backend,
    dispatch_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    prompt: &str,
    state_dir: &Path,
) -> Result<Vec<String>> {
    Ok(match backend {
        Backend::Pi => strings([
            "pi",
            "--model",
            dispatch_id,
            "--thinking",
            PI_THINKING,
            "--no-tools",
            "-p",
            prompt,
        ]),
        Backend::Omp => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("OMP dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "omp".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--thinking".to_string(),
                effort.as_str().to_string(),
                "--no-tools".to_string(),
                "--no-session".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Codex => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("Codex dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "codex".to_string(),
                "exec".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--config".to_string(),
                format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--skip-git-repo-check".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Agy => vec![
            "agy".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--add-dir".to_string(),
            state_dir.display().to_string(),
            "--model".to_string(),
            dispatch_id.to_string(),
            "--mode".to_string(),
            "plan".to_string(),
            "--sandbox".to_string(),
        ],
        Backend::Claude => strings([
            "claude",
            "--safe-mode",
            "-p",
            prompt,
            "--model",
            dispatch_id,
            "--permission-mode",
            "plan",
            "--tools",
            "",
        ]),
    })
}

fn strings<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

#[derive(Debug, Clone, Copy)]
struct ProcessRun {
    status: ProcessStatus,
    timed_out: bool,
}

fn wait_with_timeout_and_heartbeat<K>(
    child: &mut dyn ChildProcess,
    timeout: Duration,
    heartbeat_interval: Duration,
    hooks: &mut K,
) -> Result<ProcessRun>
where
    K: WorkerHooks + ?Sized,
{
    let mut elapsed = Duration::ZERO;
    let heartbeat_interval = if heartbeat_interval.is_zero() {
        WAIT_POLL
    } else {
        heartbeat_interval
    };

    loop {
        if elapsed >= timeout {
            break;
        }
        let wait = timeout.saturating_sub(elapsed).min(heartbeat_interval);
        let status = match child.wait_for(wait) {
            Ok(status) => status,
            Err(error) => {
                // A poll/wait error here (e.g. the OS call itself failing)
                // must never be mistaken for "the worker is done" — the
                // process, and any descendants in its group, could still be
                // running and writing to the repository. Terminate and reap
                // the whole group before propagating so no orphaned writer
                // can outlive the `dispatch_error` this returns.
                if terminate_and_reap_best_effort(child) {
                    return Err(error);
                }
                return Err(DispatchError::worker_state_uncertain(format!(
                    "{error}; worker process group could not be proven quiescent"
                )));
            }
        };
        if let Some(status) = status {
            ensure_child_worker_quiescent(child)?;
            return Ok(ProcessRun {
                status,
                timed_out: false,
            });
        }
        elapsed = elapsed.saturating_add(wait);
        if let Err(error) = hooks.on_heartbeat(elapsed) {
            // Same reasoning as above: a heartbeat failure (e.g. the live
            // report patch call erroring) must not leave the worker running
            // unattended after this function returns an error.
            if terminate_and_reap_best_effort(child) {
                return Err(error);
            }
            return Err(DispatchError::worker_state_uncertain(format!(
                "{error}; worker process group could not be proven quiescent"
            )));
        }
    }

    let _ = child.terminate();
    if let Ok(Some(status)) = child.wait_for(KILL_GRACE) {
        ensure_child_worker_quiescent(child)?;
        return Ok(ProcessRun {
            status,
            timed_out: true,
        });
    }

    let _ = child.kill();
    let status = child.wait()?;
    ensure_child_worker_quiescent(child)?;
    Ok(ProcessRun {
        status,
        timed_out: true,
    })
}

/// Escalates from a graceful signal to a hard kill and reaps the child,
/// swallowing every intermediate failure so a failure to signal (or to
/// observe the grace-period exit) never skips the harder escalation that
/// follows it. Used only on an already-erroring path, where the caller is
/// about to propagate a different error and cannot usefully report this
/// one too — an orphaned worker that keeps writing after Conductor has
/// moved on is worse than losing a diagnostic about noisy termination.
fn terminate_and_reap_best_effort(child: &mut dyn ChildProcess) -> bool {
    let _ = child.terminate();
    let _ = child.wait_for(KILL_GRACE);
    let _ = child.kill();
    let _ = child.wait();
    child.ensure_worker_quiescent().is_ok()
}

fn ensure_child_worker_quiescent(child: &mut dyn ChildProcess) -> Result<()> {
    child.ensure_worker_quiescent().map_err(|error| {
        DispatchError::worker_state_uncertain(format!(
            "worker process-group quiescence could not be proven: {error}"
        ))
    })
}

#[derive(Clone, Copy)]
struct CommitAuthentication<'a> {
    attempt_identity: &'a str,
    authenticated_commit: Option<&'a str>,
    requires_kernel_authentication: bool,
}

fn classify<C: CommitProbe + ?Sized>(
    process: ProcessRun,
    stdout_bytes: u64,
    before_head: Option<&str>,
    commits: &C,
    repo: &Path,
    authentication: CommitAuthentication<'_>,
) -> Result<(DispatchStatus, Option<String>)> {
    if process.timed_out {
        return Ok((DispatchStatus::Failed(DispatchFailure::TimedOut), None));
    }
    if !process.status.success {
        return Ok((
            DispatchStatus::Failed(DispatchFailure::ExitNonZero {
                code: process.status.code,
            }),
            None,
        ));
    }

    let after_head = commits.head(repo)?;
    if after_head.as_deref() != before_head {
        // A new, clean, direct-child commit is necessary but not sufficient.
        // A real worker must have reported this exact hash through the
        // post-commit socket while the kernel still proved that hook client
        // was in the current worker's process lineage. Observable environment
        // identity is retained only for in-memory test doubles, which have no
        // OS pid and therefore cannot exercise the production boundary.
        if let Some(commit) = after_head.as_deref()
            && commits.is_direct_child(repo, before_head, commit)?
            && commits.is_clean(repo)?
            && if authentication.requires_kernel_authentication {
                authentication.authenticated_commit == Some(commit)
            } else {
                commits.committer_email(repo, commit)?.as_deref()
                    == Some(authentication.attempt_identity)
            }
        {
            return Ok((DispatchStatus::Success, after_head));
        }
        return Ok((
            DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
            None,
        ));
    }
    if stdout_bytes == 0 {
        Ok((
            DispatchStatus::Failed(DispatchFailure::BackendFlakeZeroStdoutNoCommit),
            None,
        ))
    } else {
        Ok((DispatchStatus::Failed(DispatchFailure::NoNewCommit), None))
    }
}

fn file_len(path: &Path) -> Result<u64> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| DispatchError::new(format!("failed to stat {}: {e}", path.display())))
}

fn commit_receipt_socket_path(attempt_identity: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
    let mut digest = Sha256::new();
    digest.update(attempt_identity.as_bytes());
    digest.update(NEXT_SOCKET.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let mut suffix = String::with_capacity(16);
    for byte in digest.finalize().iter().take(8) {
        let _ = write!(suffix, "{byte:02x}");
    }
    std::env::temp_dir().join(format!(
        "conductor-receipt-{}-{suffix}.sock",
        std::process::id()
    ))
}

fn prepare_authenticated_commit_hook(state_dir: &Path, attempt_identity: &str) -> Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    let mut digest = Sha256::new();
    digest.update(attempt_identity.as_bytes());
    let mut name = String::with_capacity(32);
    for byte in digest.finalize().iter().take(16) {
        let _ = write!(name, "{byte:02x}");
    }
    let hook_dir = state_dir.join("worker-commit-hooks").join(name);
    fs::create_dir_all(&hook_dir).map_err(|error| {
        DispatchError::new(format!(
            "create authenticated commit hook directory {}: {error}",
            hook_dir.display()
        ))
    })?;
    let hook = hook_dir.join("post-commit");
    fs::write(
        &hook,
        br#"#!/bin/sh
commit=$(/usr/bin/git rev-parse --verify HEAD) || exit 1
reply=$(printf '%s\n' "$commit" | /usr/bin/nc -w 3 -U "$CONDUCTOR_COMMIT_RECEIPT_SOCKET" 2>/dev/null) || exit 1
[ "$reply" = "ok" ]
"#,
    )
    .map_err(|error| {
        DispatchError::new(format!(
            "write authenticated commit hook {}: {error}",
            hook.display()
        ))
    })?;
    #[cfg(unix)]
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).map_err(|error| {
        DispatchError::new(format!(
            "make authenticated commit hook executable {}: {error}",
            hook.display()
        ))
    })?;
    Ok(hook_dir)
}

#[cfg(unix)]
struct CommitReceiptBroker {
    listener: UnixListener,
    socket_path: PathBuf,
    worker_root: u32,
    accepted: Vec<String>,
}

#[cfg(unix)]
impl CommitReceiptBroker {
    fn bind(socket_path: &Path) -> Result<Self> {
        match fs::symlink_metadata(socket_path) {
            Ok(_) => {
                return Err(DispatchError::new(format!(
                    "commit receipt socket already exists: {}",
                    socket_path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DispatchError::new(format!(
                    "inspect commit receipt socket {}: {error}",
                    socket_path.display()
                )));
            }
        }
        let listener = UnixListener::bind(socket_path).map_err(|error| {
            DispatchError::new(format!(
                "bind commit receipt socket {}: {error}",
                socket_path.display()
            ))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            DispatchError::new(format!(
                "make commit receipt socket nonblocking {}: {error}",
                socket_path.display()
            ))
        })?;
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            worker_root: 0,
            accepted: Vec::new(),
        })
    }

    fn set_worker_root(&mut self, worker_root: u32) {
        self.worker_root = worker_root;
    }

    fn poll(&mut self) -> Result<()> {
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => {
                    return Err(DispatchError::new(format!(
                        "accept authenticated commit receipt: {error}"
                    )));
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .map_err(|error| DispatchError::new(format!("set receipt timeout: {error}")))?;
            let peer = peer_pid(&stream)?;
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .map_err(|error| DispatchError::new(format!("read commit receipt: {error}")))?;
            let commit = line.trim();
            if valid_git_oid(commit) && process_in_worker_session(peer, self.worker_root)? {
                self.accepted.push(commit.to_string());
                stream
                    .write_all(b"ok\n")
                    .map_err(|error| DispatchError::new(format!("ack commit receipt: {error}")))?;
            } else {
                let _ = stream.write_all(b"denied\n");
            }
        }
    }

    fn authenticated_commit(&self) -> Option<String> {
        if self.accepted.len() == 1 {
            self.accepted.first().cloned()
        } else {
            None
        }
    }
}

#[cfg(unix)]
impl Drop for CommitReceiptBroker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn valid_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
        .map_err(|error| {
            DispatchError::new(format!("authenticate commit receipt peer pid: {error}"))
        })?;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| DispatchError::new("commit receipt peer returned an invalid pid"))
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let credentials =
        nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials).map_err(
            |error| {
                DispatchError::new(format!(
                    "authenticate commit receipt peer credentials: {error}"
                ))
            },
        )?;
    u32::try_from(credentials.pid())
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| DispatchError::new("commit receipt peer returned an invalid pid"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid(_stream: &UnixStream) -> Result<u32> {
    Err(DispatchError::new(
        "kernel peer-pid authentication is unsupported on this platform",
    ))
}

fn process_in_worker_session(peer: u32, root: u32) -> Result<bool> {
    let peer = nix::unistd::Pid::from_raw(i32::try_from(peer).map_err(|error| {
        DispatchError::new(format!("convert commit receipt peer pid: {error}"))
    })?);
    let root = nix::unistd::Pid::from_raw(
        i32::try_from(root)
            .map_err(|error| DispatchError::new(format!("convert worker session pid: {error}")))?,
    );
    let peer_session = nix::unistd::getsid(Some(peer)).map_err(|error| {
        DispatchError::new(format!("authenticate commit receipt peer session: {error}"))
    })?;
    let worker_session = nix::unistd::getsid(Some(root))
        .map_err(|error| DispatchError::new(format!("authenticate worker session: {error}")))?;
    if worker_session != root {
        // The broker can receive a forged connection in the short interval
        // before the worker wrapper completes `setsid`. Deny that peer; a
        // legitimate hook can authenticate only after the boundary exists.
        return Ok(false);
    }
    Ok(peer_session == worker_session)
}

const WORKER_LINEAGE_LEASE_FILE: &str = "worker-lineage.fifo";

/// The pre-isolation FIFO path retained only to recover runs created by older
/// Conductor versions. New workers never inherit this descriptor.
pub(crate) fn worker_lineage_lease_path(run_dir: &Path) -> PathBuf {
    run_dir.join(WORKER_LINEAGE_LEASE_FILE)
}

#[cfg(unix)]
pub(crate) fn prepare_worker_lineage_lease(path: &Path) -> Result<()> {
    use std::io::ErrorKind;

    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_worker_lineage_fifo(path)?;
            if worker_lineage_active(path)? {
                return Err(DispatchError::worker_state_uncertain(format!(
                    "earlier worker lineage still holds {}",
                    path.display()
                )));
            }
            fs::remove_file(path).map_err(|error| {
                DispatchError::new(format!(
                    "remove inactive worker-lineage lease {}: {error}",
                    path.display()
                ))
            })?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DispatchError::new(format!(
                "inspect worker-lineage lease {}: {error}",
                path.display()
            )));
        }
    }

    let parent = path.parent().ok_or_else(|| {
        DispatchError::new(format!(
            "worker-lineage lease has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        DispatchError::new(format!(
            "create worker-lineage lease directory {}: {error}",
            parent.display()
        ))
    })?;
    let output = Command::new("mkfifo")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            DispatchError::new(format!(
                "spawn mkfifo for worker-lineage lease {}: {error}",
                path.display()
            ))
        })?;
    if !output.status.success() {
        return Err(DispatchError::new(format!(
            "mkfifo worker-lineage lease {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    validate_worker_lineage_fifo(path)
}

#[cfg(not(unix))]
pub(crate) fn prepare_worker_lineage_lease(_path: &Path) -> Result<()> {
    Err(DispatchError::new(
        "worker-lineage leases are only implemented on Unix",
    ))
}

#[cfg(unix)]
fn validate_worker_lineage_fifo(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;

    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DispatchError::new(format!(
            "inspect worker-lineage lease {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_fifo() {
        Ok(())
    } else {
        Err(DispatchError::worker_state_uncertain(format!(
            "worker-lineage lease is not a FIFO: {}",
            path.display()
        )))
    }
}

/// Returns whether any process still holds the inherited read end of a
/// worker's durable lineage FIFO. Opening the write end nonblocking succeeds
/// exactly while at least one reader survives; `ENXIO` proves there are none.
#[cfg(unix)]
pub(crate) fn worker_lineage_active(path: &Path) -> Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;

    validate_worker_lineage_fifo(path)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(_) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => Ok(false),
        Err(error) => Err(DispatchError::worker_state_uncertain(format!(
            "probe worker-lineage lease {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(not(unix))]
pub(crate) fn worker_lineage_active(_path: &Path) -> Result<bool> {
    Err(DispatchError::worker_state_uncertain(
        "worker-lineage lease probes are only implemented on Unix",
    ))
}

fn stdin_for_mode(mode: &StdinMode) -> Stdio {
    match mode {
        StdinMode::Null => Stdio::null(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandExec;

const WORKER_SESSION_WRAPPER: &str =
    "import os, sys; os.setsid(); os.execvpe(sys.argv[1], sys.argv[1:], os.environ)";

impl Exec for CommandExec {
    fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
        let Some((program, args)) = request.argv.split_first() else {
            return Err(DispatchError::new("cannot spawn empty argv"));
        };
        if request.sandbox_profile.is_some() {
            reject_multiply_linked_checkout_files(&request.cwd)?;
        }
        let stdout = File::create(&request.stdout_path).map_err(|e| {
            DispatchError::new(format!(
                "failed to open stdout log {}: {e}",
                request.stdout_path.display()
            ))
        })?;
        let stderr = File::create(&request.stderr_path).map_err(|e| {
            DispatchError::new(format!(
                "failed to open stderr log {}: {e}",
                request.stderr_path.display()
            ))
        })?;
        let session_isolation = request.commit_receipt_socket.is_some();
        let (target_program, target_args) = if session_isolation {
            let mut session_args = vec![
                "-c".to_string(),
                WORKER_SESSION_WRAPPER.to_string(),
                program.clone(),
            ];
            session_args.extend(args.iter().cloned());
            ("/usr/bin/python3", session_args)
        } else {
            (program.as_str(), args.to_vec())
        };
        let mut command = if let Some(profile) = &request.sandbox_profile {
            let mut command = Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-f")
                .arg(profile)
                .arg(target_program)
                .args(&target_args);
            command
        } else {
            let mut command = Command::new(target_program);
            command.args(&target_args);
            command
        };
        command
            .current_dir(&request.cwd)
            .envs(request.env.iter().map(|(key, value)| (key, value)))
            .stdin(stdin_for_mode(&request.stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if !session_isolation {
            // Non-worker subprocesses still get a dedicated process group.
            set_own_process_group(&mut command);
        }
        #[cfg(unix)]
        let mut receipt_broker = request
            .commit_receipt_socket
            .as_deref()
            .map(CommitReceiptBroker::bind)
            .transpose()?;
        let child = command.spawn().map_err(|e| {
            DispatchError::new(format!(
                "failed to spawn `{}` in {}: {e}",
                request.argv.join(" "),
                request.cwd.display()
            ))
        })?;
        #[cfg(unix)]
        if let Some(broker) = &mut receipt_broker {
            broker.set_worker_root(child.id());
        }
        Ok(Box::new(CommandChild {
            child,
            #[cfg(unix)]
            receipt_broker,
        }))
    }
}

#[cfg(unix)]
fn reject_multiply_linked_checkout_files(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            DispatchError::new(format!(
                "inspect sandbox checkout entry {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() && metadata.nlink() > 1 {
            return Err(DispatchError::new(format!(
                "sandbox checkout contains a regular file with multiple hard links: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|error| {
                DispatchError::new(format!(
                    "list sandbox checkout directory {}: {error}",
                    path.display()
                ))
            })? {
                pending.push(
                    entry
                        .map_err(|error| {
                            DispatchError::new(format!(
                                "read sandbox checkout entry in {}: {error}",
                                path.display()
                            ))
                        })?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_multiply_linked_checkout_files(_root: &Path) -> Result<()> {
    Err(DispatchError::new(
        "worker hard-link preflight is unsupported on this platform",
    ))
}

struct CommandChild {
    child: std::process::Child,
    #[cfg(unix)]
    receipt_broker: Option<CommitReceiptBroker>,
}

impl ChildProcess for CommandChild {
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
        let start = Instant::now();
        loop {
            #[cfg(unix)]
            if let Some(broker) = &mut self.receipt_broker {
                broker.poll()?;
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| DispatchError::new(format!("failed to poll child: {e}")))?
            {
                return Ok(Some(status.into()));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(remaining.min(WAIT_POLL));
        }
    }

    fn terminate(&mut self) -> Result<()> {
        send_signal_to_group(self.child.id(), "-TERM")
    }

    fn kill(&mut self) -> Result<()> {
        let result = self
            .child
            .kill()
            .map_err(|e| DispatchError::new(format!("failed to kill child: {e}")));
        // Best-effort: the direct child is authoritative for this call's
        // result (matches prior behavior exactly), but any descendants that
        // outlived it in the same process group must die too.
        let _ = send_signal_to_group(self.child.id(), "-KILL");
        result
    }

    fn wait(&mut self) -> Result<ProcessStatus> {
        self.child
            .wait()
            .map(ProcessStatus::from)
            .map_err(|e| DispatchError::new(format!("failed to wait for child: {e}")))
    }

    fn id(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn authenticated_worker_commit(&self) -> Option<String> {
        #[cfg(unix)]
        {
            self.receipt_broker
                .as_ref()
                .and_then(CommitReceiptBroker::authenticated_commit)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// Spawns the child as the leader of its own process group (`setpgid(0, 0)`
/// under the hood) so `-pid` addresses the whole group, not just this one
/// process. A safe, stable API — no `unsafe` `pre_exec` needed.
#[cfg(unix)]
fn set_own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_own_process_group(_command: &mut Command) {}

/// Sends `signal` (e.g. `"-TERM"`, `"-KILL"`) to the process *group* led by
/// `pid` — a negative pid in POSIX `kill(2)` targets the whole group — so
/// every descendant the worker spawned is reached, not just the direct
/// child. Requires the child to have been spawned via
/// [`set_own_process_group`]; harmless (targets an empty/nonexistent group)
/// otherwise.
#[cfg(unix)]
fn send_signal_to_group(pid: u32, signal: &str) -> Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| DispatchError::new(format!("failed to spawn kill {signal} -{pid}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(DispatchError::new(format!(
            "kill {signal} -{pid} failed with status {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string())
        )))
    }
}

#[cfg(not(unix))]
fn send_signal_to_group(_pid: u32, _signal: &str) -> Result<()> {
    Err(DispatchError::new(
        "process-group signal handling is only implemented on Unix",
    ))
}

#[cfg(unix)]
fn ensure_process_group_quiescent(pgid: u32) -> Result<()> {
    if !crate::quarantine::process_group_alive(pgid) {
        return Ok(());
    }

    let _ = send_signal_to_group(pgid, "-TERM");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        return Ok(());
    }

    let _ = send_signal_to_group(pgid, "-KILL");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        Ok(())
    } else {
        Err(DispatchError::worker_state_uncertain(format!(
            "worker process group {pgid} remained alive after TERM/KILL escalation"
        )))
    }
}

#[cfg(not(unix))]
fn ensure_process_group_quiescent(_pgid: u32) -> Result<()> {
    Err(DispatchError::worker_state_uncertain(
        "worker process-group quiescence is only implemented on Unix",
    ))
}

#[cfg(unix)]
fn wait_for_process_group_exit(pgid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !crate::quarantine::process_group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(WAIT_POLL);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GitCommitProbe;

impl CommitProbe for GitCommitProbe {
    fn head(&self, repo: &Path) -> Result<Option<String>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                DispatchError::new(format!(
                    "failed to run git rev-parse in {}: {e}",
                    repo.display()
                ))
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if head.is_empty() {
            Ok(None)
        } else {
            Ok(Some(head))
        }
    }

    fn is_clean(&self, repo: &Path) -> Result<bool> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["status", "--porcelain", "--untracked-files=normal"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                DispatchError::new(format!(
                    "failed to run git status in {}: {error}",
                    repo.display()
                ))
            })?;
        if !output.status.success() {
            return Err(DispatchError::new(format!(
                "git status failed in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout.is_empty())
    }

    fn is_direct_child(&self, repo: &Path, before: Option<&str>, commit: &str) -> Result<bool> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--parents", "-n", "1", commit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                DispatchError::new(format!(
                    "failed to inspect commit parents in {}: {e}",
                    repo.display()
                ))
            })?;
        if !output.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut fields = stdout.split_whitespace();
        if fields.next() != Some(commit) {
            return Ok(false);
        }
        Ok(match before {
            Some(parent) => fields.next() == Some(parent) && fields.next().is_none(),
            None => fields.next().is_none(),
        })
    }

    fn committer_email(&self, repo: &Path, commit: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", "--no-patch", "--format=%ce", commit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                DispatchError::new(format!(
                    "failed to read committer identity in {}: {e}",
                    repo.display()
                ))
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let email = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!email.is_empty()).then_some(email))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const BEFORE_COMMIT: &str = "1111111111111111111111111111111111111111";
    const WORKER_COMMIT: &str = "2222222222222222222222222222222222222222";
    const WORKER_STDOUT: &str = "worker stdout\n";

    /// Adapts a bare heartbeat closure to the [`WorkerHooks`] trait so the
    /// wait-loop tests can drive `on_heartbeat` without a full observer.
    struct HeartbeatFn<F>(F);

    impl<F: FnMut(Duration) -> Result<()>> WorkerHooks for HeartbeatFn<F> {
        fn on_heartbeat(&mut self, elapsed: Duration) -> Result<()> {
            (self.0)(elapsed)
        }
    }

    #[test]
    fn pi_backend_uses_pinned_argv_repo_cwd_and_receipt_socket() {
        let temp = TempDir::new("pi-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(result.status, DispatchStatus::Success);
        assert_eq!(result.worker_commit.as_deref(), Some(WORKER_COMMIT));
        let spawn = exec.spawned();
        assert_eq!(
            spawn.argv,
            vec![
                "pi",
                "--model",
                "opencode-go/glm-5.2",
                "--thinking",
                "xhigh",
                "--approve",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(spawn.cwd, repo);
        assert_eq!(spawn.stdin, StdinMode::Null);
        assert_eq!(spawn.sandbox_profile, None);
        assert!(spawn.commit_receipt_socket.is_some());
        assert_eq!(
            spawn.stdout_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.out")
        );
        assert_eq!(
            spawn.stderr_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.err")
        );
        let receipt_socket = spawn
            .commit_receipt_socket
            .as_ref()
            .expect("receipt socket")
            .display()
            .to_string();
        let env = spawn
            .env
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(env["GIT_AUTHOR_NAME"], ATTEMPT_IDENTITY_NAME);
        assert_eq!(env["GIT_AUTHOR_EMAIL"], TEST_ATTEMPT_IDENTITY);
        assert_eq!(env["GIT_COMMITTER_NAME"], ATTEMPT_IDENTITY_NAME);
        assert_eq!(env["GIT_COMMITTER_EMAIL"], TEST_ATTEMPT_IDENTITY);
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "core.hooksPath");
        assert!(
            Path::new(&env["GIT_CONFIG_VALUE_0"])
                .join("post-commit")
                .exists()
        );
        assert_eq!(env["CONDUCTOR_COMMIT_RECEIPT_SOCKET"], receipt_socket);
    }

    #[test]
    fn codex_backend_uses_per_run_reasoning_override() {
        let temp = TempDir::new("codex-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let mut request = request(&repo, Backend::Codex, "gpt-5.6-sol", Some(BEFORE_COMMIT));
        request.reasoning_effort = Some(ReasoningEffort::Max);
        request.sandbox_profile = Some(repo.join("worker.sb"));

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "codex",
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5.6-sol",
                "--config",
                "model_reasoning_effort=\"max\"",
                PROMPT,
            ]
        );
    }

    #[test]
    fn missing_outer_sandbox_does_not_disable_harness_safety() {
        let temp = TempDir::new("missing-outer-sandbox");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut codex_request =
            request(&repo, Backend::Codex, "gpt-5.6-sol", Some(BEFORE_COMMIT));
        codex_request.reasoning_effort = Some(ReasoningEffort::Max);
        let codex = spawn_request(&codex_request, temp.path())
            .expect("build Codex spawn without outer sandbox");
        assert!(
            !codex
                .argv
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
            "Codex's sandbox must remain enabled when no outer profile exists"
        );

        let claude = spawn_request(
            &request(
                &repo,
                Backend::Claude,
                "claude-sonnet-5",
                Some(BEFORE_COMMIT),
            ),
            temp.path(),
        )
        .expect("build Claude spawn without outer sandbox");
        assert!(
            !claude
                .argv
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "Claude's permission checks must remain enabled when no outer profile exists"
        );
    }

    #[test]
    fn agy_backend_uses_pinned_argv_with_load_bearing_add_dir() {
        let temp = TempDir::new("agy-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Agy,
            "Gemini 3.5 Flash (High)",
            Some(BEFORE_COMMIT),
        );

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "agy",
                "-p",
                PROMPT,
                "--add-dir",
                repo.to_str().expect("utf8 repo"),
                "--model",
                "Gemini 3.5 Flash (High)",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn claude_backend_uses_pinned_argv() {
        let temp = TempDir::new("claude-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let mut request = request(
            &repo,
            Backend::Claude,
            "claude-sonnet-5",
            Some(BEFORE_COMMIT),
        );
        request.sandbox_profile = Some(repo.join("worker.sb"));

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "claude",
                "-p",
                PROMPT,
                "--model",
                "claude-sonnet-5",
                "--dangerously-skip-permissions"
            ]
        );
    }

    #[test]
    fn adversarial_readonly_argv_disables_tools_for_every_backend() {
        let repo = Path::new("/tmp/review-state");

        assert_eq!(
            readonly_argv_for_backend(Backend::Pi, "opencode-go/glm-5.2", None, PROMPT, repo,)
                .expect("pi readonly argv"),
            vec![
                "pi",
                "--model",
                "opencode-go/glm-5.2",
                "--thinking",
                "xhigh",
                "--no-tools",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-terra",
                Some(ReasoningEffort::Xhigh),
                PROMPT,
                repo,
            )
            .expect("OMP readonly argv"),
            vec![
                "omp",
                "--model",
                "openai-codex/gpt-5.6-terra",
                "--thinking",
                "xhigh",
                "--no-tools",
                "--no-session",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(
                Backend::Codex,
                "gpt-5.6-terra",
                Some(ReasoningEffort::Xhigh),
                PROMPT,
                repo,
            )
            .expect("codex readonly argv"),
            vec![
                "codex",
                "exec",
                "--model",
                "gpt-5.6-terra",
                "--config",
                "model_reasoning_effort=\"xhigh\"",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(Backend::Agy, "Gemini 3.5 Flash (High)", None, PROMPT, repo,)
                .expect("agy readonly argv"),
            vec![
                "agy",
                "-p",
                PROMPT,
                "--add-dir",
                "/tmp/review-state",
                "--model",
                "Gemini 3.5 Flash (High)",
                "--mode",
                "plan",
                "--sandbox",
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(Backend::Claude, "claude-sonnet-5", None, PROMPT, repo,)
                .expect("claude readonly argv"),
            vec![
                "claude",
                "--safe-mode",
                "-p",
                PROMPT,
                "--model",
                "claude-sonnet-5",
                "--permission-mode",
                "plan",
                "--tools",
                "",
            ]
        );
    }

    #[test]
    fn omp_backend_uses_explicit_ephemeral_unattended_argv() {
        assert_eq!(
            argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-luna",
                Some(ReasoningEffort::Medium),
                PROMPT,
                Path::new("/tmp/repo"),
            )
            .expect("OMP argv"),
            vec![
                "omp",
                "--model",
                "openai-codex/gpt-5.6-luna",
                "--thinking",
                "medium",
                "--auto-approve",
                "--no-session",
                "-p",
                PROMPT,
            ]
        );
        assert!(
            argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-luna",
                None,
                PROMPT,
                Path::new("/tmp/repo"),
            )
            .is_err(),
            "OMP dispatch must not inherit a global thinking level"
        );
    }

    #[test]
    fn timeout_path_sends_term_then_waits_grace_then_kills() {
        let temp = TempDir::new("timeout");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::timeout_then_kill();
        let commits = FakeCommits::new([Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("timeout is reported as dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::TimedOut)
        );
        assert_eq!(
            exec.events(),
            vec![
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(Duration::from_secs(3)),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    #[test]
    fn wait_for_error_terminates_and_reaps_the_process_group_before_propagating() {
        // A `wait_for` failure (e.g. the OS poll call itself erroring) must
        // never be mistaken for "the worker finished" — it, and any
        // descendants in its process group, could still be running. The
        // group must be terminated and reaped before the error propagates,
        // not after.
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut child = FakeChild::wait_for_error(Rc::clone(&events));

        let error = wait_with_timeout_and_heartbeat(
            &mut child,
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut (),
        )
        .expect_err("a wait_for error must propagate, not be swallowed");

        assert_eq!(error.to_string(), "simulated wait_for failure");
        assert_eq!(
            events.borrow().as_slice(),
            [
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(KILL_GRACE),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    #[test]
    fn heartbeat_error_terminates_and_reaps_the_process_group_before_propagating() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut child = FakeChild::pending(Rc::clone(&events));

        let error = wait_with_timeout_and_heartbeat(
            &mut child,
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut HeartbeatFn(|_elapsed: Duration| {
                Err(DispatchError::new("simulated heartbeat failure"))
            }),
        )
        .expect_err("a heartbeat error must propagate, not be swallowed");

        assert_eq!(error.to_string(), "simulated heartbeat failure");
        assert_eq!(
            events.borrow().as_slice(),
            [
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(KILL_GRACE),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    /// Records call order across [`WorkerHooks::on_pre_spawn`] and
    /// [`Exec::spawn`] into a single shared log, proving the invalidate step
    /// truly happens before the worker process exists rather than merely
    /// before `on_spawn`.
    struct OrderingHooks {
        log: Rc<RefCell<Vec<&'static str>>>,
        pre_spawn_error: Option<&'static str>,
    }

    impl WorkerHooks for OrderingHooks {
        fn on_pre_spawn(&mut self) -> Result<()> {
            self.log.borrow_mut().push("pre_spawn");
            match self.pre_spawn_error {
                None => Ok(()),
                Some(message) => Err(DispatchError::new(message)),
            }
        }
    }

    struct OrderingExec {
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Exec for OrderingExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            self.log.borrow_mut().push("spawn");
            std::fs::write(&request.stdout_path, b"").expect("write fake stdout");
            std::fs::write(&request.stderr_path, b"").expect("write fake stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    #[test]
    fn on_pre_spawn_runs_before_the_worker_is_spawned() {
        let temp = TempDir::new("pre-spawn-order");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let log = Rc::new(RefCell::new(Vec::new()));
        let exec = OrderingExec {
            log: Rc::clone(&log),
        };
        let commits = FakeCommits::new([Some("before"), Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));
        let mut hooks = OrderingHooks {
            log: Rc::clone(&log),
            pre_spawn_error: None,
        };

        run_with_heartbeat(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut hooks,
        )
        .expect("dispatch succeeds");

        assert_eq!(
            log.borrow().as_slice(),
            ["pre_spawn", "spawn"],
            "the prior attempt's identity must be invalidated before the new worker exists"
        );
    }

    #[test]
    fn on_pre_spawn_failure_prevents_the_spawn_entirely() {
        let temp = TempDir::new("pre-spawn-failure");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let log = Rc::new(RefCell::new(Vec::new()));
        let exec = OrderingExec {
            log: Rc::clone(&log),
        };
        let commits = FakeCommits::new([Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));
        let mut hooks = OrderingHooks {
            log: Rc::clone(&log),
            pre_spawn_error: Some("simulated invalidate failure"),
        };

        let error = run_with_heartbeat(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut hooks,
        )
        .expect_err("a failed invalidation must prevent the worker from ever running");

        assert_eq!(error.to_string(), "simulated invalidate failure");
        assert_eq!(
            log.borrow().as_slice(),
            ["pre_spawn"],
            "the worker must never spawn once identity invalidation has failed"
        );
    }

    #[test]
    fn stdout_and_stderr_logs_are_written_under_cycle_and_bead() {
        let temp = TempDir::new("logs");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "worker stderr\n");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            result.stdout_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.out")
        );
        assert_eq!(
            result.stderr_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.err")
        );
        assert_eq!(
            std::fs::read_to_string(&result.stdout_path).unwrap(),
            WORKER_STDOUT
        );
        assert_eq!(
            std::fs::read_to_string(&result.stderr_path).unwrap(),
            "worker stderr\n"
        );
        assert_eq!(result.stdout_bytes, WORKER_STDOUT.len() as u64);
        assert_eq!(result.stderr_bytes, 14);
    }

    #[test]
    fn exit_zero_with_no_new_commit_and_zero_stdout_is_backend_flake_failure() {
        let temp = TempDir::new("zero-stdout-no-commit");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("", "");
        let commits = FakeCommits::new([Some("same"), Some("same")]);
        let request = request(&repo, Backend::Agy, "Gemini 3.5 Flash (High)", Some("same"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::BackendFlakeZeroStdoutNoCommit)
        );
        assert_eq!(result.stdout_bytes, 0);
    }

    #[test]
    fn exit_zero_with_no_new_commit_and_nonzero_stdout_is_no_new_commit_failure() {
        let temp = TempDir::new("nonzero-stdout-no-commit");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker tried\n", "");
        let commits = FakeCommits::new([Some("same"), Some("same")]);
        let request = request(&repo, Backend::Claude, "claude-sonnet-5", Some("same"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::NoNewCommit)
        );
        assert_eq!(result.stdout_bytes, 13);
    }

    #[test]
    fn exit_zero_with_foreign_head_change_is_not_worker_success() {
        let temp = TempDir::new("foreign-head");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(
            "CONDUCTOR_WORKER_COMMIT: 2222222222222222222222222222222222222222\n",
            "",
        );
        let commits = FakeCommits::new([
            Some("1111111111111111111111111111111111111111"),
            Some("3333333333333333333333333333333333333333"),
        ]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some("1111111111111111111111111111111111111111"),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert!(
            !matches!(result.status, DispatchStatus::Success),
            "a foreign HEAD change must not authenticate worker success"
        );
    }

    #[test]
    fn parent_observes_a_clean_direct_child_in_the_attempt_checkout() {
        let temp = TempDir::new("observed-direct-child");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Conductor Test"]);
        git(
            &repo,
            &["config", "user.email", "conductor-test@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        let before_head = git(&repo, &["rev-parse", "HEAD"]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(&before_head),
        );

        let result = run(
            &DirectChildExec,
            &GitCommitProbe,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(result.status, DispatchStatus::Success);
        assert_eq!(
            result.worker_commit.as_deref(),
            Some(git(&repo, &["rev-parse", "HEAD"]).as_str())
        );
    }

    #[test]
    fn foreign_commit_inserted_before_worker_commit_is_not_worker_success() {
        let temp = TempDir::new("foreign-parent");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Conductor Test"]);
        git(
            &repo,
            &["config", "user.email", "conductor-test@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        let before_head = git(&repo, &["rev-parse", "HEAD"]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(&before_head),
        );

        let result = run(
            &ForeignThenWorkerExec,
            &GitCommitProbe,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert!(
            !matches!(result.status, DispatchStatus::Success),
            "a foreign commit inserted between the base and worker commit must not authenticate success"
        );
    }

    const PROMPT: &str = "work on the bead";
    const TEST_ATTEMPT_IDENTITY: &str = "conductor-attempt-test@invalid";

    fn request(
        repo: &Path,
        backend: Backend,
        dispatch_id: &str,
        before_head: Option<&str>,
    ) -> DispatchRequest {
        DispatchRequest {
            repo: repo.to_path_buf(),
            before_head: before_head.map(str::to_string),
            attempt_id: "001-worker".to_string(),
            cycle_id: "cycle-1".to_string(),
            bead_id: "bead-1".to_string(),
            backend,
            dispatch_id: dispatch_id.to_string(),
            reasoning_effort: None,
            prompt: PROMPT.to_string(),
            attempt_identity: TEST_ATTEMPT_IDENTITY.to_string(),
            sandbox_profile: None,
        }
    }

    #[derive(Clone)]
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-dispatch-{label}-{nanos}"));
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

    struct FakeExec {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        child: RefCell<Option<FakeChild>>,
        spawned: RefCell<Option<SpawnRequest>>,
        events: Rc<RefCell<Vec<ExecEvent>>>,
    }

    impl FakeExec {
        fn success(stdout: &str, stderr: &str) -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                child: RefCell::new(Some(FakeChild::success(Rc::clone(&events)))),
                spawned: RefCell::new(None),
                events,
            }
        }

        fn timeout_then_kill() -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
                child: RefCell::new(Some(FakeChild::timeout_then_kill(Rc::clone(&events)))),
                spawned: RefCell::new(None),
                events,
            }
        }

        fn spawned(&self) -> SpawnRequest {
            self.spawned.borrow().as_ref().expect("spawned").clone()
        }

        fn events(&self) -> Vec<ExecEvent> {
            self.events.borrow().clone()
        }
    }

    struct ForeignThenWorkerExec;

    struct DirectChildExec;

    impl Exec for DirectChildExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(request.cwd.join("worker.txt"), b"worker\n")
                .expect("write worker change");
            git_as_worker(request, &["add", "worker.txt"]);
            git_as_worker(request, &["commit", "-m", "worker: clean direct child"]);
            std::fs::write(&request.stdout_path, b"worker complete\n")
                .expect("write worker stdout");
            std::fs::write(&request.stderr_path, b"").expect("write worker stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    impl Exec for ForeignThenWorkerExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(request.cwd.join("foreign.txt"), b"foreign\n")
                .expect("write foreign change");
            git(&request.cwd, &["add", "foreign.txt"]);
            git(
                &request.cwd,
                &["commit", "-m", "foreign: concurrent change"],
            );

            std::fs::write(request.cwd.join("worker.txt"), b"worker\n")
                .expect("write worker change");
            git(&request.cwd, &["add", "worker.txt"]);
            git(&request.cwd, &["commit", "-m", "worker: intended change"]);
            let worker_commit = git(&request.cwd, &["rev-parse", "HEAD"]);

            std::fs::write(
                &request.stdout_path,
                format!("CONDUCTOR_WORKER_COMMIT: {worker_commit}\n"),
            )
            .expect("write worker stdout");
            std::fs::write(&request.stderr_path, b"").expect("write worker stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    /// Runs git under the spawn environment so in-memory test doubles carry
    /// the audit identity used by their non-OS authentication fallback.
    fn git_as_worker(request: &SpawnRequest, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&request.cwd)
            .args(args)
            .envs(request.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn git as worker");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    impl Exec for FakeExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(&request.stdout_path, &self.stdout).expect("write fake stdout");
            std::fs::write(&request.stderr_path, &self.stderr).expect("write fake stderr");
            *self.spawned.borrow_mut() = Some(request.clone());
            let child = self.child.borrow_mut().take().expect("one spawn");
            Ok(Box::new(child))
        }
    }

    struct FakeChild {
        events: Rc<RefCell<Vec<ExecEvent>>>,
        wait_for_results: RefCell<Vec<Option<ProcessStatus>>>,
        /// 0-indexed `wait_for` call number that should return `Err` instead
        /// of popping `wait_for_results` — used to prove the caller reaps
        /// the process group rather than leaving it running on error.
        wait_for_error_at_call: Option<usize>,
        wait_for_calls: usize,
        wait_result: ProcessStatus,
    }

    impl FakeChild {
        fn success(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::code(0),
            }
        }

        fn timeout_then_kill(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![None, None]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }

        /// The very first `wait_for` call fails outright — simulates an OS
        /// poll error while the worker may still be running.
        fn wait_for_error(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: Some(0),
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }

        /// The first `wait_for` call reports "still running" (`None`) so a
        /// caller-supplied heartbeat closure gets invoked next.
        fn pending(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![None, Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }
    }

    impl ChildProcess for FakeChild {
        fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
            self.events.borrow_mut().push(ExecEvent::WaitFor(timeout));
            let call = self.wait_for_calls;
            self.wait_for_calls += 1;
            if self.wait_for_error_at_call == Some(call) {
                return Err(DispatchError::new("simulated wait_for failure"));
            }
            Ok(self.wait_for_results.borrow_mut().remove(0))
        }

        fn terminate(&mut self) -> Result<()> {
            self.events.borrow_mut().push(ExecEvent::Terminate);
            Ok(())
        }

        fn kill(&mut self) -> Result<()> {
            self.events.borrow_mut().push(ExecEvent::Kill);
            Ok(())
        }

        fn wait(&mut self) -> Result<ProcessStatus> {
            self.events.borrow_mut().push(ExecEvent::Wait);
            Ok(self.wait_result)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ExecEvent {
        WaitFor(Duration),
        Terminate,
        Kill,
        Wait,
    }

    struct FakeCommits {
        heads: RefCell<Vec<Option<String>>>,
    }

    impl FakeCommits {
        fn new<const N: usize>(heads: [Option<&str>; N]) -> Self {
            Self {
                heads: RefCell::new(heads.into_iter().map(|h| h.map(str::to_string)).collect()),
            }
        }
    }

    impl CommitProbe for FakeCommits {
        fn head(&self, _repo: &Path) -> Result<Option<String>> {
            Ok(self.heads.borrow_mut().remove(0))
        }

        fn is_clean(&self, _repo: &Path) -> Result<bool> {
            Ok(true)
        }

        fn is_direct_child(
            &self,
            _repo: &Path,
            _before: Option<&str>,
            commit: &str,
        ) -> Result<bool> {
            Ok(matches!(commit, WORKER_COMMIT | "after"))
        }

        fn committer_email(&self, _repo: &Path, _commit: &str) -> Result<Option<String>> {
            Ok(Some(TEST_ATTEMPT_IDENTITY.to_string()))
        }
    }

    #[test]
    #[cfg(unix)]
    fn command_exec_kill_terminates_descendant_processes_in_the_group() {
        // A worker CLI can fork children of its own (subshells, tool
        // invocations); if the timeout path only kills the direct child, a
        // grandchild can outlive it and keep writing to the repository
        // after Conductor has already declared the tree state. Spawning the
        // worker as the leader of its own process group and signaling
        // `-pid` on timeout must reach every descendant, not just the one
        // process std::process::Child knows about directly.
        let temp = TempDir::new("process-group-kill");
        let marker = temp.path().join("grandchild.pid");
        let request = SpawnRequest {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("sleep 30 & echo $! > {}; wait", marker.display()),
            ],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            commit_receipt_socket: None,
            stdout_path: temp.path().join("out.log"),
            stderr_path: temp.path().join("err.log"),
        };

        let exec = CommandExec;
        let mut child = exec.spawn(&request).expect("spawn worker shell");
        let grandchild_pid = wait_for_pid_marker(&marker);
        assert!(
            process_alive(grandchild_pid),
            "precondition: grandchild must actually be running before we try to kill it"
        );

        child.kill().expect("kill direct child");
        let _ = child.wait();

        assert!(
            !process_alive(grandchild_pid),
            "grandchild process must not survive killing the process group"
        );
    }

    #[cfg(unix)]
    fn wait_for_pid_marker(marker: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "grandchild never wrote its pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Polls briefly since signal delivery/reaping is not synchronous with
    /// the `kill` call returning.
    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn kill -0 probe");
            if !status.success() {
                return false;
            }
            if Instant::now() >= deadline {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
