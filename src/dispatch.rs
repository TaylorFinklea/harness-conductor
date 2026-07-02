//! backend runners (pi/agy/claude) behind a trait (Exec) + timeout/kill

// Built ahead of the M4 integration path; unit tests exercise this module directly.
#![allow(dead_code)]

use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::config::Backend;

const PI_THINKING: &str = "xhigh";
const KILL_GRACE: Duration = Duration::from_secs(3);
const WAIT_POLL: Duration = Duration::from_millis(50);

pub(crate) type Result<T> = std::result::Result<T, DispatchError>;

#[derive(Debug, Clone)]
pub(crate) struct DispatchError {
    message: String,
}

impl DispatchError {
    fn new(message: impl Into<String>) -> Self {
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
}

pub(crate) trait CommitProbe {
    fn head(&self, repo: &Path) -> Result<Option<String>>;
}

pub(crate) fn run<E: Exec, C: CommitProbe>(
    exec: &E,
    commits: &C,
    request: &DispatchRequest,
    state_dir: &Path,
    timeout: Duration,
) -> Result<DispatchResult> {
    let before_head = commits.head(&request.repo)?;
    let spawn = spawn_request(request, state_dir)?;
    let mut child = exec.spawn(&spawn)?;
    let process = wait_with_timeout(child.as_mut(), timeout)?;
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
            &request.prompt,
            &request.repo,
        ),
        cwd: request.repo.clone(),
        stdin: StdinMode::Null,
        stdout_path,
        stderr_path,
    })
}

fn argv_for_backend(backend: Backend, dispatch_id: &str, prompt: &str, repo: &Path) -> Vec<String> {
    match backend {
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
    }
}

fn strings<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

#[derive(Clone, Copy)]
struct ProcessRun {
    status: ProcessStatus,
    timed_out: bool,
}

fn wait_with_timeout(child: &mut dyn ChildProcess, timeout: Duration) -> Result<ProcessRun> {
    if let Some(status) = child.wait_for(timeout)? {
        return Ok(ProcessRun {
            status,
            timed_out: false,
        });
    }

    child.terminate()?;
    if let Some(status) = child.wait_for(KILL_GRACE)? {
        return Ok(ProcessRun {
            status,
            timed_out: true,
        });
    }

    child.kill()?;
    let status = child.wait()?;
    Ok(ProcessRun {
        status,
        timed_out: true,
    })
}

fn classify<C: CommitProbe>(
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
        let child = Command::new(program)
            .args(args)
            .current_dir(&request.cwd)
            .stdin(match request.stdin {
                StdinMode::Null => Stdio::null(),
            })
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| {
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
        send_sigterm(self.child.id())
    }

    fn kill(&mut self) -> Result<()> {
        self.child
            .kill()
            .map_err(|e| DispatchError::new(format!("failed to kill child: {e}")))
    }

    fn wait(&mut self) -> Result<ProcessStatus> {
        self.child
            .wait()
            .map(ProcessStatus::from)
            .map_err(|e| DispatchError::new(format!("failed to wait for child: {e}")))
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| DispatchError::new(format!("failed to spawn kill -TERM {pid}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(DispatchError::new(format!(
            "kill -TERM {pid} failed with status {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string())
        )))
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) -> Result<()> {
    Err(DispatchError::new(
        "SIGTERM timeout handling is only implemented on Unix",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        wait_result: ProcessStatus,
    }

    impl FakeChild {
        fn success(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![Some(ProcessStatus::code(0))]),
                wait_result: ProcessStatus::code(0),
            }
        }

        fn timeout_then_kill(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![None, None]),
                wait_result: ProcessStatus::signal(),
            }
        }
    }

    impl ChildProcess for FakeChild {
        fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
            self.events.borrow_mut().push(ExecEvent::WaitFor(timeout));
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
    }
}
