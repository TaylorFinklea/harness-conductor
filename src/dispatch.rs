//! backend runners (pi/agy/claude/codex) behind a trait (Exec) + timeout/kill

// Built ahead of the M4 integration path; unit tests exercise this module directly.
#![allow(dead_code)]

use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Backend, ReasoningEffort};

const PI_THINKING: &str = "xhigh";
const KILL_GRACE: Duration = Duration::from_secs(3);
const WAIT_POLL: Duration = Duration::from_millis(50);

pub(crate) type Result<T> = std::result::Result<T, DispatchError>;

#[derive(Debug, Clone)]
pub(crate) struct DispatchError {
    message: String,
}

impl DispatchError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    pub(crate) cycle_id: String,
    pub(crate) bead_id: String,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchResult {
    pub(crate) status: DispatchStatus,
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
    BackendFlakeZeroStdoutNoCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) stdin: StdinMode,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Callbacks the worker runtime invokes around a dispatched worker's lifetime.
/// A single observer (rather than separate closures) so it can hold one
/// exclusive borrow of the run's durable state across both the one-shot
/// spawn hook and the repeated heartbeat ticks.
pub(crate) trait WorkerHooks {
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
    let before_head = commits.head(&request.repo)?;
    let spawn = spawn_request(request, state_dir)?;
    let mut child = exec.spawn(&spawn)?;
    // Bind the run to this worker's process group before it can meaningfully
    // mutate the repository. If that durable record fails, tear the worker
    // (and any descendants) down rather than let a worker whose identity we
    // cannot prove keep running unattended.
    if let Err(error) = hooks.on_spawn(child.id()) {
        terminate_and_reap_best_effort(child.as_mut());
        return Err(error);
    }
    let process =
        wait_with_timeout_and_heartbeat(child.as_mut(), timeout, heartbeat_interval, hooks)?;
    let stdout_bytes = file_len(&spawn.stdout_path)?;
    let stderr_bytes = file_len(&spawn.stderr_path)?;
    let status = classify(
        process,
        stdout_bytes,
        before_head.as_deref(),
        commits,
        &request.repo,
    )?;

    Ok(DispatchResult {
        status,
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
    let stdout_path = log_dir.join(format!("{}.out", request.bead_id));
    let stderr_path = log_dir.join(format!("{}.err", request.bead_id));
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

    Ok(SpawnRequest {
        argv: argv_for_backend(
            request.backend,
            &request.dispatch_id,
            request.reasoning_effort,
            &request.prompt,
            &request.repo,
        )?,
        cwd: request.repo.clone(),
        stdin: StdinMode::Null,
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
                terminate_and_reap_best_effort(child);
                return Err(error);
            }
        };
        if let Some(status) = status {
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
            terminate_and_reap_best_effort(child);
            return Err(error);
        }
    }

    let _ = child.terminate();
    if let Ok(Some(status)) = child.wait_for(KILL_GRACE) {
        return Ok(ProcessRun {
            status,
            timed_out: true,
        });
    }

    let _ = child.kill();
    let status = child.wait()?;
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
fn terminate_and_reap_best_effort(child: &mut dyn ChildProcess) {
    let _ = child.terminate();
    let _ = child.wait_for(KILL_GRACE);
    let _ = child.kill();
    let _ = child.wait();
}

fn classify<C: CommitProbe + ?Sized>(
    process: ProcessRun,
    stdout_bytes: u64,
    before_head: Option<&str>,
    commits: &C,
    repo: &Path,
) -> Result<DispatchStatus> {
    if process.timed_out {
        return Ok(DispatchStatus::Failed(DispatchFailure::TimedOut));
    }
    if !process.status.success {
        return Ok(DispatchStatus::Failed(DispatchFailure::ExitNonZero {
            code: process.status.code,
        }));
    }

    let after_head = commits.head(repo)?;
    if after_head.as_deref() != before_head {
        return Ok(DispatchStatus::Success);
    }
    if stdout_bytes == 0 {
        Ok(DispatchStatus::Failed(
            DispatchFailure::BackendFlakeZeroStdoutNoCommit,
        ))
    } else {
        Ok(DispatchStatus::Failed(DispatchFailure::NoNewCommit))
    }
}

fn file_len(path: &Path) -> Result<u64> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| DispatchError::new(format!("failed to stat {}: {e}", path.display())))
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandExec;

impl Exec for CommandExec {
    fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
        let Some((program, args)) = request.argv.split_first() else {
            return Err(DispatchError::new("cannot spawn empty argv"));
        };
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
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&request.cwd)
            .stdin(match request.stdin {
                StdinMode::Null => Stdio::null(),
            })
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // Make the worker the leader of its own process group so a timeout
        // can terminate every descendant it spawned, not just the direct
        // child — otherwise a grandchild process can keep writing to the
        // repository after Conductor has already declared the tree state
        // and moved on to quarantine capture or a clean check.
        set_own_process_group(&mut command);
        let child = command.spawn().map_err(|e| {
            DispatchError::new(format!(
                "failed to spawn `{}` in {}: {e}",
                request.argv.join(" "),
                request.cwd.display()
            ))
        })?;
        Ok(Box::new(CommandChild { child }))
    }
}

struct CommandChild {
    child: std::process::Child,
}

impl ChildProcess for CommandChild {
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
        let start = Instant::now();
        loop {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Adapts a bare heartbeat closure to the [`WorkerHooks`] trait so the
    /// wait-loop tests can drive `on_heartbeat` without a full observer.
    struct HeartbeatFn<F>(F);

    impl<F: FnMut(Duration) -> Result<()>> WorkerHooks for HeartbeatFn<F> {
        fn on_heartbeat(&mut self, elapsed: Duration) -> Result<()> {
            (self.0)(elapsed)
        }
    }

    #[test]
    fn pi_backend_uses_pinned_argv_repo_cwd_and_null_stdin() {
        let temp = TempDir::new("pi-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker stdout\n", "");
        let commits = FakeCommits::new([Some("before"), Some("after")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2");

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(result.status, DispatchStatus::Success);
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
        assert_eq!(
            spawn.stdout_path,
            temp.path().join("logs/cycle-1/bead-1.out")
        );
        assert_eq!(
            spawn.stderr_path,
            temp.path().join("logs/cycle-1/bead-1.err")
        );
    }

    #[test]
    fn codex_backend_uses_per_run_reasoning_override() {
        let temp = TempDir::new("codex-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker stdout\n", "");
        let commits = FakeCommits::new([Some("before"), Some("after")]);
        let mut request = request(&repo, Backend::Codex, "gpt-5.6-sol");
        request.reasoning_effort = Some(ReasoningEffort::Max);

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
                "--model",
                "gpt-5.6-sol",
                "--config",
                "model_reasoning_effort=\"max\"",
                PROMPT,
            ]
        );
    }

    #[test]
    fn agy_backend_uses_pinned_argv_with_load_bearing_add_dir() {
        let temp = TempDir::new("agy-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker stdout\n", "");
        let commits = FakeCommits::new([Some("before"), Some("after")]);
        let request = request(&repo, Backend::Agy, "Gemini 3.5 Flash (High)");

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
        let exec = FakeExec::success("worker stdout\n", "");
        let commits = FakeCommits::new([Some("before"), Some("after")]);
        let request = request(&repo, Backend::Claude, "claude-sonnet-5");

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
            vec!["claude", "-p", PROMPT, "--model", "claude-sonnet-5"]
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
    fn timeout_path_sends_term_then_waits_grace_then_kills() {
        let temp = TempDir::new("timeout");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::timeout_then_kill();
        let commits = FakeCommits::new([Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2");

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

    #[test]
    fn stdout_and_stderr_logs_are_written_under_cycle_and_bead() {
        let temp = TempDir::new("logs");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker stdout\n", "worker stderr\n");
        let commits = FakeCommits::new([Some("before"), Some("after")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2");

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
            temp.path().join("logs/cycle-1/bead-1.out")
        );
        assert_eq!(
            result.stderr_path,
            temp.path().join("logs/cycle-1/bead-1.err")
        );
        assert_eq!(
            std::fs::read_to_string(&result.stdout_path).unwrap(),
            "worker stdout\n"
        );
        assert_eq!(
            std::fs::read_to_string(&result.stderr_path).unwrap(),
            "worker stderr\n"
        );
        assert_eq!(result.stdout_bytes, 14);
        assert_eq!(result.stderr_bytes, 14);
    }

    #[test]
    fn exit_zero_with_no_new_commit_and_zero_stdout_is_backend_flake_failure() {
        let temp = TempDir::new("zero-stdout-no-commit");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("", "");
        let commits = FakeCommits::new([Some("same"), Some("same")]);
        let request = request(&repo, Backend::Agy, "Gemini 3.5 Flash (High)");

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
        let request = request(&repo, Backend::Claude, "claude-sonnet-5");

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

    const PROMPT: &str = "work on the bead";

    fn request(repo: &Path, backend: Backend, dispatch_id: &str) -> DispatchRequest {
        DispatchRequest {
            repo: repo.to_path_buf(),
            cycle_id: "cycle-1".to_string(),
            bead_id: "bead-1".to_string(),
            backend,
            dispatch_id: dispatch_id.to_string(),
            reasoning_effort: None,
            prompt: PROMPT.to_string(),
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
            stdin: StdinMode::Null,
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
