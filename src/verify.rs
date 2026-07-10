//! `verify_cmd` runner + orchestra subprocess + close/release decisions

#![allow(dead_code)]

use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::bd::BdClient;
use crate::config::{Efficiency, ReviewConfig, RosterEntry, Tier, VerifyConfig};
use crate::dispatch::{
    CommitProbe, DispatchFailure, DispatchStatus, Exec, ProcessStatus, SpawnRequest, StdinMode,
};

const ORCHESTRA_RETRY_BACKOFF: Duration = Duration::from_secs(1);

pub(crate) type Result<T> = std::result::Result<T, VerifyError>;

#[derive(Debug, Clone)]
pub(crate) struct VerifyError {
    message: String,
}

impl VerifyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VerifyError {}

impl From<crate::dispatch::DispatchError> for VerifyError {
    fn from(value: crate::dispatch::DispatchError) -> Self {
        Self::new(value.to_string())
    }
}

impl From<crate::bd::BdError> for VerifyError {
    fn from(value: crate::bd::BdError) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VerifyRequest {
    pub(crate) repo: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) cycle_id: String,
    pub(crate) issue: crate::bd::Issue,
    pub(crate) verify_cmd: String,
    pub(crate) verify: VerifyConfig,
    pub(crate) worker_status: DispatchStatus,
    pub(crate) before_head: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewSettings {
    pub(crate) config: ReviewConfig,
    pub(crate) roster: Vec<RosterEntry>,
    pub(crate) dispatched_model: RosterEntry,
    pub(crate) item_tier_floor: Tier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewRecord {
    pub(crate) model: String,
    pub(crate) verify_passed: bool,
    pub(crate) summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifyOutcome {
    pub(crate) decision: VerifyDecision,
    pub(crate) verify_passed: bool,
    pub(crate) summary: String,
    pub(crate) review_dispatches: u64,
    pub(crate) review: Option<ReviewRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyDecision {
    Passed,
    Failed,
    HardError,
}

pub(crate) fn run<B: BdClient + ?Sized, E: Exec + ?Sized, C: CommitProbe + ?Sized>(
    bd: &B,
    exec: &E,
    commits: &C,
    request: &VerifyRequest,
) -> Result<VerifyOutcome> {
    run_with_optional_review_backoff(bd, exec, commits, request, None, ORCHESTRA_RETRY_BACKOFF)
}

pub(crate) fn run_with_review<B: BdClient + ?Sized, E: Exec + ?Sized, C: CommitProbe + ?Sized>(
    bd: &B,
    exec: &E,
    commits: &C,
    request: &VerifyRequest,
    review: &ReviewSettings,
) -> Result<VerifyOutcome> {
    run_with_optional_review_backoff(
        bd,
        exec,
        commits,
        request,
        Some(review),
        ORCHESTRA_RETRY_BACKOFF,
    )
}

fn run_with_backoff<B: BdClient + ?Sized, E: Exec + ?Sized, C: CommitProbe + ?Sized>(
    bd: &B,
    exec: &E,
    commits: &C,
    request: &VerifyRequest,
    retry_backoff: Duration,
) -> Result<VerifyOutcome> {
    run_with_optional_review_backoff(bd, exec, commits, request, None, retry_backoff)
}

fn run_with_review_backoff<B: BdClient + ?Sized, E: Exec + ?Sized, C: CommitProbe + ?Sized>(
    bd: &B,
    exec: &E,
    commits: &C,
    request: &VerifyRequest,
    review: &ReviewSettings,
    retry_backoff: Duration,
) -> Result<VerifyOutcome> {
    run_with_optional_review_backoff(bd, exec, commits, request, Some(review), retry_backoff)
}

fn run_with_optional_review_backoff<
    B: BdClient + ?Sized,
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
>(
    bd: &B,
    exec: &E,
    commits: &C,
    request: &VerifyRequest,
    review: Option<&ReviewSettings>,
    retry_backoff: Duration,
) -> Result<VerifyOutcome> {
    if let Some(summary) = worker_failure_summary(&request.worker_status) {
        return fail(bd, request, VerifyDecision::Failed, summary);
    }

    let after_head = commits.head(&request.repo)?;
    if !has_new_commit(request.before_head.as_deref(), after_head.as_deref()) {
        return fail(
            bd,
            request,
            VerifyDecision::Failed,
            "no new commit after worker".to_string(),
        );
    }

    let verify_run = run_spawn(exec, &verify_spawn(request)?)?;
    if !verify_run.status.success() {
        return fail(
            bd,
            request,
            VerifyDecision::Failed,
            format!(
                "verify_cmd failed with {}",
                status_summary(verify_run.status)
            ),
        );
    }

    if should_run_orchestra(request) {
        match run_orchestra_with_retry(exec, request, retry_backoff)? {
            OrchestraDecision::Passed => review_or_pass(bd, exec, request, review),
            OrchestraDecision::Failed(summary) => {
                fail(bd, request, VerifyDecision::Failed, summary)
            }
            OrchestraDecision::HardError(summary) => {
                fail(bd, request, VerifyDecision::HardError, summary)
            }
        }
    } else {
        review_or_pass(bd, exec, request, review)
    }
}

fn worker_failure_summary(status: &DispatchStatus) -> Option<String> {
    match status {
        DispatchStatus::Success => None,
        DispatchStatus::Failed(failure) => Some(format!(
            "worker failed: {}",
            dispatch_failure_summary(failure)
        )),
    }
}

fn dispatch_failure_summary(failure: &DispatchFailure) -> String {
    match failure {
        DispatchFailure::TimedOut => "timed out".to_string(),
        DispatchFailure::ExitNonZero { code } => code.map_or_else(
            || "terminated by signal".to_string(),
            |code| format!("exit {code}"),
        ),
        DispatchFailure::NoNewCommit => "no new commit".to_string(),
        DispatchFailure::BackendFlakeZeroStdoutNoCommit => {
            "backend flake: zero stdout and no new commit".to_string()
        }
    }
}

fn has_new_commit(before: Option<&str>, after: Option<&str>) -> bool {
    after.is_some() && after != before
}

fn should_run_orchestra(request: &VerifyRequest) -> bool {
    request.verify.always_orchestra || adversarial_metadata(&request.issue)
}

fn adversarial_metadata(issue: &crate::bd::Issue) -> bool {
    issue
        .metadata
        .as_ref()
        .and_then(|m| m.get("adversarial"))
        .is_some_and(|v| match v {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
            _ => false,
        })
}

fn pass<B: BdClient + ?Sized>(bd: &B, request: &VerifyRequest) -> Result<VerifyOutcome> {
    pass_with_review(bd, request, 0, None)
}

fn pass_with_review<B: BdClient + ?Sized>(
    bd: &B,
    request: &VerifyRequest,
    review_dispatches: u64,
    review: Option<ReviewRecord>,
) -> Result<VerifyOutcome> {
    let reason = format!(
        "conductor {}: verified via {}",
        request.cycle_id, request.verify_cmd
    );
    bd.close(&request.repo, &request.issue.id, &reason)?;
    Ok(VerifyOutcome {
        decision: VerifyDecision::Passed,
        verify_passed: true,
        summary: reason,
        review_dispatches,
        review,
    })
}

fn fail<B: BdClient + ?Sized>(
    bd: &B,
    request: &VerifyRequest,
    decision: VerifyDecision,
    summary: String,
) -> Result<VerifyOutcome> {
    fail_with_review(bd, request, decision, summary, 0, None)
}

fn fail_with_review<B: BdClient + ?Sized>(
    bd: &B,
    request: &VerifyRequest,
    decision: VerifyDecision,
    summary: String,
    review_dispatches: u64,
    review: Option<ReviewRecord>,
) -> Result<VerifyOutcome> {
    bd.release(&request.repo, &request.issue.id)?;
    let comment = format!(
        "conductor: {} {} verify failed: {}",
        request.cycle_id, request.issue.id, summary
    );
    bd.comment(&request.repo, &request.issue.id, &comment)?;
    Ok(VerifyOutcome {
        decision,
        verify_passed: false,
        summary,
        review_dispatches,
        review,
    })
}

#[derive(Debug, Clone)]
struct CommandRun {
    status: ProcessStatus,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

fn run_spawn<E: Exec + ?Sized>(exec: &E, spawn: &SpawnRequest) -> Result<CommandRun> {
    let stdout_path = spawn.stdout_path.clone();
    let stderr_path = spawn.stderr_path.clone();
    let mut child = exec.spawn(spawn)?;
    let status = child.wait()?;
    Ok(CommandRun {
        status,
        stdout_path,
        stderr_path,
    })
}

fn verify_spawn(request: &VerifyRequest) -> Result<SpawnRequest> {
    spawn_request(
        request,
        "verify",
        vec![
            "sh".to_string(),
            "-c".to_string(),
            request.verify_cmd.clone(),
        ],
    )
}

fn orchestra_spawn(request: &VerifyRequest, suffix: &str) -> Result<SpawnRequest> {
    let claim = format!(
        "{}: {}",
        request.issue.title, request.issue.acceptance_criteria
    );
    spawn_request(
        request,
        suffix,
        vec![
            "orchestra".to_string(),
            "verify".to_string(),
            claim,
            "--evidence".to_string(),
            request.verify_cmd.clone(),
            "--model".to_string(),
            request.verify.judge.clone(),
            "--cwd".to_string(),
            request.repo.display().to_string(),
        ],
    )
}

fn review_spawn(
    request: &VerifyRequest,
    reviewer: &RosterEntry,
    prompt: &str,
) -> Result<SpawnRequest> {
    spawn_request(
        request,
        "review",
        crate::dispatch::argv_for_backend(
            reviewer.backend,
            &reviewer.dispatch_id,
            reviewer.reasoning_effort,
            prompt,
            &request.repo,
        )
        .map_err(|error| VerifyError::new(error.to_string()))?,
    )
}

fn spawn_request(request: &VerifyRequest, suffix: &str, argv: Vec<String>) -> Result<SpawnRequest> {
    let log_dir = request.state_dir.join("logs").join(&request.cycle_id);
    fs::create_dir_all(&log_dir).map_err(|e| {
        VerifyError::new(format!(
            "failed to create verify log dir {}: {e}",
            log_dir.display()
        ))
    })?;
    let stdout_path = log_dir.join(format!("{}.{}.out", request.issue.id, suffix));
    let stderr_path = log_dir.join(format!("{}.{}.err", request.issue.id, suffix));
    touch(&stdout_path)?;
    touch(&stderr_path)?;
    Ok(SpawnRequest {
        argv,
        cwd: request.repo.clone(),
        stdin: StdinMode::Null,
        stdout_path,
        stderr_path,
    })
}

fn touch(path: &Path) -> Result<()> {
    File::create(path)
        .map(|_| ())
        .map_err(|e| VerifyError::new(format!("failed to create log {}: {e}", path.display())))
}

enum ReviewDecision {
    NotNeeded,
    Ship(ReviewRecord),
    Revise {
        record: ReviewRecord,
        findings: Vec<String>,
    },
    Failed {
        dispatches: u64,
        record: Option<ReviewRecord>,
        summary: String,
    },
}

#[derive(Debug, Deserialize)]
struct ReviewVerdict {
    verdict: ReviewVerdictKind,
    findings: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReviewVerdictKind {
    Ship,
    Revise,
}

fn review_or_pass<B: BdClient + ?Sized, E: Exec + ?Sized>(
    bd: &B,
    exec: &E,
    request: &VerifyRequest,
    review: Option<&ReviewSettings>,
) -> Result<VerifyOutcome> {
    let Some(settings) = review else {
        return pass(bd, request);
    };

    match run_review(exec, request, settings)? {
        ReviewDecision::NotNeeded => pass(bd, request),
        ReviewDecision::Ship(record) => pass_with_review(bd, request, 1, Some(record)),
        ReviewDecision::Revise { record, findings } => {
            review_revise(bd, request, record, &findings)
        }
        ReviewDecision::Failed {
            dispatches,
            record,
            summary,
        } => fail_with_review(
            bd,
            request,
            VerifyDecision::Failed,
            summary,
            dispatches,
            record,
        ),
    }
}

fn review_revise<B: BdClient + ?Sized>(
    bd: &B,
    request: &VerifyRequest,
    record: ReviewRecord,
    findings: &[String],
) -> Result<VerifyOutcome> {
    bd.release(&request.repo, &request.issue.id)?;
    let summary = review_findings_summary(findings);
    let comment = format!(
        "conductor: {} {} qualitative review requested revisions:\n{}",
        request.cycle_id,
        request.issue.id,
        review_findings_bullets(findings)
    );
    bd.comment(&request.repo, &request.issue.id, &comment)?;
    Ok(VerifyOutcome {
        decision: VerifyDecision::Failed,
        verify_passed: false,
        summary,
        review_dispatches: 1,
        review: Some(record),
    })
}

fn run_review<E: Exec + ?Sized>(
    exec: &E,
    request: &VerifyRequest,
    settings: &ReviewSettings,
) -> Result<ReviewDecision> {
    let reviewer = match reviewer_for(settings) {
        ReviewerSelection::NotNeeded => return Ok(ReviewDecision::NotNeeded),
        ReviewerSelection::Reviewer(reviewer) => reviewer,
        ReviewerSelection::MissingReviewer(floor) => {
            return Ok(ReviewDecision::Failed {
                dispatches: 0,
                record: None,
                summary: format!(
                    "qualitative review required but no {floor:?}-or-higher reviewer is rostered"
                ),
            });
        }
    };
    let prompt = review_prompt(request, settings, reviewer);
    let run = run_spawn(exec, &review_spawn(request, reviewer, &prompt)?)?;
    if !run.status.success() {
        let summary = format!(
            "qualitative review failed with {}: {}",
            status_summary(run.status),
            summarize_file(&run.stderr_path)
        );
        return Ok(ReviewDecision::Failed {
            dispatches: 1,
            record: Some(review_record(reviewer, false, &summary)),
            summary,
        });
    }

    let verdict = match parse_review_verdict(&run.stdout_path) {
        Ok(verdict) => verdict,
        Err(summary) => {
            return Ok(ReviewDecision::Failed {
                dispatches: 1,
                record: Some(review_record(reviewer, false, &summary)),
                summary,
            });
        }
    };
    match verdict.verdict {
        ReviewVerdictKind::Ship => {
            let summary = "qualitative review verdict: ship".to_string();
            Ok(ReviewDecision::Ship(review_record(
                reviewer, true, &summary,
            )))
        }
        ReviewVerdictKind::Revise => {
            let summary = review_findings_summary(&verdict.findings);
            Ok(ReviewDecision::Revise {
                record: review_record(reviewer, false, &summary),
                findings: verdict.findings,
            })
        }
    }
}

enum ReviewerSelection<'a> {
    NotNeeded,
    Reviewer(&'a RosterEntry),
    MissingReviewer(Tier),
}

fn reviewer_for(settings: &ReviewSettings) -> ReviewerSelection<'_> {
    if !settings.config.enabled {
        return ReviewerSelection::NotNeeded;
    }
    let review_ceiling = review_ceiling(settings.item_tier_floor);
    let gap = tier_rank(review_ceiling).saturating_sub(tier_rank(settings.dispatched_model.tier));
    if gap == 0 || u32::from(gap) < settings.config.min_tier_gap {
        return ReviewerSelection::NotNeeded;
    }
    select_reviewer(&settings.roster, review_ceiling).map_or(
        ReviewerSelection::MissingReviewer(review_ceiling),
        ReviewerSelection::Reviewer,
    )
}

fn review_ceiling(tier_floor: Tier) -> Tier {
    match tier_floor {
        Tier::Junior => Tier::Senior,
        Tier::Senior | Tier::Lead => Tier::Lead,
    }
}

fn select_reviewer(roster: &[RosterEntry], floor: Tier) -> Option<&RosterEntry> {
    let mut qualifying: Vec<(usize, &RosterEntry)> = roster
        .iter()
        .enumerate()
        .filter(|(_, entry)| tier_rank(entry.tier) >= tier_rank(floor))
        .collect();
    if qualifying.is_empty() {
        return None;
    }
    let min_tier = qualifying
        .iter()
        .map(|(_, entry)| tier_rank(entry.tier))
        .min()?;
    qualifying.retain(|(_, entry)| tier_rank(entry.tier) == min_tier);
    let min_efficiency = qualifying
        .iter()
        .map(|(_, entry)| efficiency_rank(entry.efficiency))
        .min()?;
    qualifying.retain(|(_, entry)| efficiency_rank(entry.efficiency) == min_efficiency);
    qualifying.sort_by_key(|(index, _)| *index);
    qualifying.first().map(|(_, entry)| *entry)
}

fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Junior => 0,
        Tier::Senior => 1,
        Tier::Lead => 2,
    }
}

fn efficiency_rank(efficiency: Efficiency) -> u8 {
    match efficiency {
        Efficiency::Lean => 0,
        Efficiency::Std => 1,
        Efficiency::Heavy => 2,
    }
}

fn review_prompt(
    request: &VerifyRequest,
    settings: &ReviewSettings,
    reviewer: &RosterEntry,
) -> String {
    format!(
        "READ-ONLY qualitative review for Conductor.\n\
         Reviewer model: {}\n\
         Worker model: {}\n\
         Repo: {}\n\
         Bead: {} — {}\n\
         Description:\n{}\n\n\
         Acceptance criteria:\n{}\n\n\
         Notes:\n{}\n\n\
         Mechanical verify passed with: {}\n\n\
         Do not edit files, run bd mutations, claim, close, commit, push, or change state.\n\
         Return ONLY compact JSON with this exact schema: \
         {{\"verdict\":\"ship\"|\"revise\",\"findings\":[\"...\"]}}.\n\
         Use verdict=ship only if the work is ready to close; otherwise verdict=revise with actionable findings.",
        reviewer.name,
        settings.dispatched_model.name,
        request.repo.display(),
        request.issue.id,
        request.issue.title,
        request.issue.description,
        request.issue.acceptance_criteria,
        request.issue.notes,
        request.verify_cmd
    )
}

fn parse_review_verdict(path: &Path) -> std::result::Result<ReviewVerdict, String> {
    let stdout = fs::read_to_string(path).map_err(|e| {
        format!(
            "failed to read qualitative review stdout {}: {e}",
            path.display()
        )
    })?;
    serde_json::from_str(&stdout).map_err(|e| {
        format!(
            "invalid qualitative review verdict JSON in {}: {e}",
            path.display()
        )
    })
}

fn review_record(reviewer: &RosterEntry, verify_passed: bool, summary: &str) -> ReviewRecord {
    ReviewRecord {
        model: reviewer.name.clone(),
        verify_passed,
        summary: summary.to_string(),
    }
}

fn review_findings_summary(findings: &[String]) -> String {
    if findings.is_empty() {
        "qualitative review requested revisions".to_string()
    } else {
        format!(
            "qualitative review requested revisions: {}",
            findings.join("; ")
        )
    }
}

fn review_findings_bullets(findings: &[String]) -> String {
    if findings.is_empty() {
        return "- <no findings supplied>".to_string();
    }
    findings
        .iter()
        .map(|finding| format!("- {finding}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn summarize_file(path: &Path) -> String {
    fs::read_to_string(path).map_or_else(
        |e| format!("failed to read {}: {e}", path.display()),
        |content| summarize_stderr(&content),
    )
}

enum OrchestraDecision {
    Passed,
    Failed(String),
    HardError(String),
}

enum OrchestraAttempt {
    Passed,
    Failed(String),
    HardError(String),
    Wedged,
}

fn run_orchestra_with_retry<E: Exec + ?Sized>(
    exec: &E,
    request: &VerifyRequest,
    retry_backoff: Duration,
) -> Result<OrchestraDecision> {
    match run_orchestra_attempt(exec, request, "orchestra")? {
        OrchestraAttempt::Passed => Ok(OrchestraDecision::Passed),
        OrchestraAttempt::Failed(summary) => Ok(OrchestraDecision::Failed(summary)),
        OrchestraAttempt::HardError(summary) => Ok(OrchestraDecision::HardError(summary)),
        OrchestraAttempt::Wedged => {
            if !retry_backoff.is_zero() {
                std::thread::sleep(retry_backoff);
            }
            match run_orchestra_attempt(exec, request, "orchestra-retry")? {
                OrchestraAttempt::Passed => Ok(OrchestraDecision::Passed),
                OrchestraAttempt::Failed(summary) => Ok(OrchestraDecision::Failed(summary)),
                OrchestraAttempt::HardError(summary) => Ok(OrchestraDecision::HardError(summary)),
                OrchestraAttempt::Wedged => Ok(OrchestraDecision::Failed(
                    "orchestra endpoint likely wedged after retry".to_string(),
                )),
            }
        }
    }
}

fn run_orchestra_attempt<E: Exec + ?Sized>(
    exec: &E,
    request: &VerifyRequest,
    suffix: &str,
) -> Result<OrchestraAttempt> {
    let run = run_spawn(exec, &orchestra_spawn(request, suffix)?)?;
    classify_orchestra(&run)
}

fn classify_orchestra(run: &CommandRun) -> Result<OrchestraAttempt> {
    if run.status.success() {
        return Ok(OrchestraAttempt::Passed);
    }

    let stderr = fs::read_to_string(&run.stderr_path).map_err(|e| {
        VerifyError::new(format!(
            "failed to read orchestra stderr {}: {e}",
            run.stderr_path.display()
        ))
    })?;
    match run.status.exit_code() {
        Some(1) => Ok(OrchestraAttempt::Failed(
            "orchestra verify failed with exit 1".to_string(),
        )),
        Some(2) if stderr.trim_start().starts_with("usage:") => Ok(OrchestraAttempt::HardError(
            format!("orchestra usage error: {}", summarize_stderr(&stderr)),
        )),
        Some(2) if stderr.contains("endpoint likely wedged") => Ok(OrchestraAttempt::Wedged),
        Some(2) => Ok(OrchestraAttempt::Failed(format!(
            "orchestra verify errored with exit 2: {}",
            summarize_stderr(&stderr)
        ))),
        Some(code) => Ok(OrchestraAttempt::Failed(format!(
            "orchestra verify failed with exit {code}"
        ))),
        None => Ok(OrchestraAttempt::Failed(
            "orchestra verify terminated by signal".to_string(),
        )),
    }
}

fn status_summary(status: ProcessStatus) -> String {
    status
        .exit_code()
        .map_or_else(|| "signal".to_string(), |code| format!("exit {code}"))
}

fn summarize_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "<empty stderr>".to_string()
    } else {
        trimmed.lines().next().unwrap_or(trimmed).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdClient, BdError, Comment, Issue};
    use crate::config::{
        Backend, Ceiling, Cost, Efficiency, ReviewConfig, RosterEntry, Tier, VerifyConfig,
    };
    use crate::dispatch::{
        ChildProcess, CommitProbe, DispatchFailure, DispatchStatus, Exec, ProcessStatus,
        SpawnRequest, StdinMode,
    };
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn verify_passes_closes_bead_when_new_commit_and_verify_cmd_succeeds_without_orchestra() {
        let temp = TempDir::new("pass-no-orchestra");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![Process::exit(0, "verify ok\n", "")]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline succeeds");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert!(outcome.verify_passed);
        assert_eq!(
            bd.events(),
            vec![BdEvent::Close {
                repo: request.repo.clone(),
                id: "bead-1".to_string(),
                reason: "conductor cycle-1: verified via cargo test verify".to_string(),
            }]
        );
        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].argv, vec!["sh", "-c", "cargo test verify"]);
        assert_eq!(spawns[0].cwd, request.repo);
        assert_eq!(spawns[0].stdin, StdinMode::Null);
    }

    #[test]
    fn verify_fails_releases_and_comments_when_worker_created_no_new_commit() {
        let temp = TempDir::new("no-new-commit");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("same"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![]);
        let commits = FakeCommits::new([Some("same")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline reports failure");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert!(!outcome.verify_passed);
        assert!(
            exec.spawns().is_empty(),
            "verify_cmd must not run without a new commit"
        );
        assert_release_then_comment_contains(&bd.events(), &request.repo, "no new commit");
    }

    #[test]
    fn verify_fails_releases_and_comments_when_worker_did_not_exit_cleanly() {
        let temp = TempDir::new("worker-failed");
        let mut request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        request.worker_status = DispatchStatus::Failed(DispatchFailure::TimedOut);
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![]);
        let commits = FakeCommits::new([]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline reports worker failure");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert!(
            exec.spawns().is_empty(),
            "post-worker commands must not run after timeout"
        );
        assert_release_then_comment_contains(&bd.events(), &request.repo, "timed out");
    }

    #[test]
    fn verify_cmd_nonzero_releases_and_comments_without_closing() {
        let temp = TempDir::new("verify-nonzero");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![Process::exit(42, "", "test failed\n")]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline reports verify_cmd failure");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert_release_then_comment_contains(
            &bd.events(),
            &request.repo,
            "verify_cmd failed with exit 42",
        );
    }

    #[test]
    fn always_orchestra_runs_oracle_with_pinned_model_and_closes_on_pass() {
        let temp = TempDir::new("always-orchestra-pass");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(true),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(0, "[PASS] confidence 5\n", ""),
        ]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline passes");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert_eq!(bd.close_count(), 1);
        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 2);
        assert_eq!(
            spawns[1].argv,
            vec![
                "orchestra",
                "verify",
                "Implement feature: acceptance criteria",
                "--evidence",
                "cargo test verify",
                "--model",
                "opencode-go/qwen3.7-max",
                "--cwd",
                request.repo.to_str().expect("utf8 repo"),
            ]
        );
        assert_eq!(spawns[1].cwd, request.repo);
        assert_eq!(spawns[1].stdin, StdinMode::Null);
    }

    #[test]
    fn adversarial_metadata_triggers_orchestra_even_when_config_does_not_force_it() {
        let temp = TempDir::new("adversarial-orchestra");
        let request = request(
            temp.path(),
            issue(true),
            verify_config(false),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![Process::exit(0, "", ""), Process::exit(0, "", "")]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline passes");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert_eq!(
            exec.spawns().len(),
            2,
            "orchestra must run for adversarial beads"
        );
    }

    #[test]
    fn review_triggers_only_when_dispatched_tier_is_below_review_ceiling() {
        let temp = TempDir::new("review-trigger-threshold");
        let review_request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let roster = review_roster();
        let bd = FakeBdClient::new(&review_request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(0, r#"{"verdict":"ship","findings":[]}"#, ""),
        ]);
        let commits = FakeCommits::new([Some("after")]);
        let settings = ReviewSettings {
            config: ReviewConfig {
                enabled: true,
                min_tier_gap: 1,
            },
            roster: roster.clone(),
            dispatched_model: roster[0].clone(),
            item_tier_floor: Tier::Junior,
        };

        let outcome = run_with_review_backoff(
            &bd,
            &exec,
            &commits,
            &review_request,
            &settings,
            Duration::ZERO,
        )
        .expect("reviewed verify pipeline passes");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert_eq!(outcome.review_dispatches, 1);
        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 2);
        assert_eq!(spawns[1].argv[0], "pi");
        assert!(spawns[1].argv.contains(&"senior-reviewer".to_string()));
        assert_eq!(bd.close_count(), 1);

        let no_review_temp = TempDir::new("review-no-threshold");
        let no_review_request = request(
            no_review_temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let no_review_bd = FakeBdClient::new(&no_review_request.issue);
        let no_review_exec = FakeExec::new(vec![Process::exit(0, "verify ok\n", "")]);
        let no_review_commits = FakeCommits::new([Some("after")]);
        let no_review_settings = ReviewSettings {
            config: ReviewConfig {
                enabled: true,
                min_tier_gap: 1,
            },
            roster: roster.clone(),
            dispatched_model: roster[1].clone(),
            item_tier_floor: Tier::Junior,
        };

        let no_review_outcome = run_with_review_backoff(
            &no_review_bd,
            &no_review_exec,
            &no_review_commits,
            &no_review_request,
            &no_review_settings,
            Duration::ZERO,
        )
        .expect("verify pipeline without review passes");

        assert_eq!(no_review_outcome.decision, VerifyDecision::Passed);
        assert_eq!(no_review_outcome.review_dispatches, 0);
        assert_eq!(no_review_exec.spawns().len(), 1);
        assert_eq!(no_review_bd.close_count(), 1);
    }

    #[test]
    fn review_revise_holds_bead_comments_findings_and_releases_claim() {
        let temp = TempDir::new("review-revise");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let roster = review_roster();
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(
                0,
                r#"{"verdict":"revise","findings":["missing edge-case test","scope drift"]}"#,
                "",
            ),
        ]);
        let commits = FakeCommits::new([Some("after")]);
        let settings = ReviewSettings {
            config: ReviewConfig {
                enabled: true,
                min_tier_gap: 1,
            },
            roster,
            dispatched_model: review_roster()[0].clone(),
            item_tier_floor: Tier::Junior,
        };

        let outcome =
            run_with_review_backoff(&bd, &exec, &commits, &request, &settings, Duration::ZERO)
                .expect("review revise is a normal verify outcome");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert!(!outcome.verify_passed);
        assert_eq!(outcome.review_dispatches, 1);
        assert_eq!(bd.close_count(), 0);
        assert_release_then_comment_contains(&bd.events(), &request.repo, "missing edge-case test");
        assert_release_then_comment_contains(&bd.events(), &request.repo, "scope drift");
    }

    #[test]
    fn review_config_flag_disables_review_and_closes_after_mechanical_verify() {
        let temp = TempDir::new("review-disabled");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(false),
            Some("before"),
        );
        let roster = review_roster();
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![Process::exit(0, "verify ok\n", "")]);
        let commits = FakeCommits::new([Some("after")]);
        let settings = ReviewSettings {
            config: ReviewConfig {
                enabled: false,
                min_tier_gap: 1,
            },
            roster,
            dispatched_model: review_roster()[0].clone(),
            item_tier_floor: Tier::Junior,
        };

        let outcome =
            run_with_review_backoff(&bd, &exec, &commits, &request, &settings, Duration::ZERO)
                .expect("verify pipeline passes without review when disabled");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert_eq!(outcome.review_dispatches, 0);
        assert_eq!(exec.spawns().len(), 1);
        assert_eq!(bd.close_count(), 1);
    }

    #[test]
    fn orchestra_exit_one_releases_and_comments() {
        let temp = TempDir::new("orchestra-fail");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(true),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(1, "[FAIL] confidence 4\n", "model rejected evidence\n"),
        ]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("verify pipeline reports oracle failure");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert_release_then_comment_contains(
            &bd.events(),
            &request.repo,
            "orchestra verify failed with exit 1",
        );
    }

    #[test]
    fn orchestra_exit_two_usage_prefix_is_hard_error_without_retry() {
        let temp = TempDir::new("orchestra-usage");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(true),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(2, "", "usage: orchestra verify <claim>\n"),
        ]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("usage is reported as hard error decision");

        assert_eq!(outcome.decision, VerifyDecision::HardError);
        assert!(!outcome.verify_passed);
        assert_eq!(exec.spawns().len(), 2, "usage errors must not retry");
        assert_release_then_comment_contains(&bd.events(), &request.repo, "orchestra usage error");
    }

    #[test]
    fn orchestra_exit_two_wedged_retries_once_then_closes_if_retry_passes() {
        let temp = TempDir::new("orchestra-wedged-pass");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(true),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(2, "", "opencode-go endpoint likely wedged\n"),
            Process::exit(0, "[PASS] confidence 4\n", ""),
        ]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("retry pass closes");

        assert_eq!(outcome.decision, VerifyDecision::Passed);
        assert_eq!(exec.spawns().len(), 3, "one retry after wedged exit 2");
        assert_eq!(bd.close_count(), 1);
    }

    #[test]
    fn orchestra_exit_two_wedged_retries_once_then_releases_if_retry_is_still_wedged() {
        let temp = TempDir::new("orchestra-wedged-fail");
        let request = request(
            temp.path(),
            issue(false),
            verify_config(true),
            Some("before"),
        );
        let bd = FakeBdClient::new(&request.issue);
        let exec = FakeExec::new(vec![
            Process::exit(0, "verify ok\n", ""),
            Process::exit(2, "", "opencode-go endpoint likely wedged\n"),
            Process::exit(2, "", "opencode-go endpoint likely wedged\n"),
        ]);
        let commits = FakeCommits::new([Some("after")]);

        let outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
            .expect("retry exhaustion is a normal failure");

        assert_eq!(outcome.decision, VerifyDecision::Failed);
        assert_eq!(exec.spawns().len(), 3, "only one retry is allowed");
        assert_release_then_comment_contains(
            &bd.events(),
            &request.repo,
            "endpoint likely wedged after retry",
        );
    }

    #[test]
    fn invariant_6_close_only_after_worker_new_commit_verify_and_required_orchestra_all_pass() {
        struct Case {
            name: &'static str,
            worker_status: DispatchStatus,
            after_head: Option<&'static str>,
            exec: Vec<Process>,
            always_orchestra: bool,
            expected_close_count: usize,
        }

        let cases = vec![
            Case {
                name: "worker timeout",
                worker_status: DispatchStatus::Failed(DispatchFailure::TimedOut),
                after_head: None,
                exec: vec![],
                always_orchestra: false,
                expected_close_count: 0,
            },
            Case {
                name: "no new commit",
                worker_status: DispatchStatus::Success,
                after_head: Some("before"),
                exec: vec![],
                always_orchestra: false,
                expected_close_count: 0,
            },
            Case {
                name: "verify_cmd fails",
                worker_status: DispatchStatus::Success,
                after_head: Some("after"),
                exec: vec![Process::exit(1, "", "")],
                always_orchestra: false,
                expected_close_count: 0,
            },
            Case {
                name: "orchestra fails",
                worker_status: DispatchStatus::Success,
                after_head: Some("after"),
                exec: vec![Process::exit(0, "", ""), Process::exit(1, "", "")],
                always_orchestra: true,
                expected_close_count: 0,
            },
            Case {
                name: "all gates pass",
                worker_status: DispatchStatus::Success,
                after_head: Some("after"),
                exec: vec![Process::exit(0, "", ""), Process::exit(0, "", "")],
                always_orchestra: true,
                expected_close_count: 1,
            },
        ];

        for case in cases {
            let temp = TempDir::new(case.name);
            let mut request = request(
                temp.path(),
                issue(false),
                verify_config(case.always_orchestra),
                Some("before"),
            );
            request.worker_status = case.worker_status;
            let bd = FakeBdClient::new(&request.issue);
            let exec = FakeExec::new(case.exec);
            let commits = match case.after_head {
                Some(head) => FakeCommits::new([Some(head)]),
                None => FakeCommits::new([]),
            };

            let _outcome = run_with_backoff(&bd, &exec, &commits, &request, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{}: pipeline errored: {e}", case.name));

            assert_eq!(
                bd.close_count(),
                case.expected_close_count,
                "{}: bd close must fire only after all invariant-6 gates pass",
                case.name
            );
        }
    }

    fn request(
        temp: &Path,
        issue: Issue,
        verify: VerifyConfig,
        before_head: Option<&str>,
    ) -> VerifyRequest {
        let repo = temp.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        VerifyRequest {
            repo,
            state_dir: temp.join("state"),
            cycle_id: "cycle-1".to_string(),
            issue,
            verify_cmd: "cargo test verify".to_string(),
            verify,
            worker_status: DispatchStatus::Success,
            before_head: before_head.map(str::to_string),
        }
    }

    fn verify_config(always_orchestra: bool) -> VerifyConfig {
        VerifyConfig {
            judge: "opencode-go/qwen3.7-max".to_string(),
            always_orchestra,
        }
    }

    fn review_roster() -> Vec<RosterEntry> {
        vec![
            roster_entry(
                "junior-worker",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Agy,
                "junior-worker",
            ),
            roster_entry(
                "senior-reviewer",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
                "senior-reviewer",
            ),
            roster_entry(
                "lead-reviewer",
                Tier::Lead,
                Ceiling::L,
                Efficiency::Std,
                Backend::Claude,
                "lead-reviewer",
            ),
        ]
    }

    fn roster_entry(
        name: &str,
        tier: Tier,
        ceiling: Ceiling,
        efficiency: Efficiency,
        backend: Backend,
        dispatch_id: &str,
    ) -> RosterEntry {
        RosterEntry {
            name: name.to_string(),
            tier,
            ceiling,
            efficiency,
            backend,
            dispatch_id: dispatch_id.to_string(),
            reasoning_effort: None,
            provider: String::new(),
            cost: Cost::Paid,
            fallback: Vec::new(),
        }
    }

    fn issue(adversarial: bool) -> Issue {
        let metadata = adversarial.then(|| {
            let mut metadata = BTreeMap::new();
            metadata.insert("adversarial".to_string(), json!(true));
            metadata
        });
        Issue {
            id: "bead-1".to_string(),
            title: "Implement feature".to_string(),
            description: String::new(),
            acceptance_criteria: "acceptance criteria".to_string(),
            notes: String::new(),
            status: "in_progress".to_string(),
            priority: 1,
            issue_type: "task".to_string(),
            assignee: Some("conductor".to_string()),
            owner: "test".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: "2026-07-02T00:00:00Z".to_string(),
            started_at: Some("2026-07-02T00:00:00Z".to_string()),
            labels: None,
            estimated_minutes: None,
            metadata,
            parent: None,
            dependencies: None,
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

    fn assert_release_then_comment_contains(
        events: &[BdEvent],
        repo: &Path,
        expected_summary: &str,
    ) {
        assert_eq!(
            events.len(),
            2,
            "expected release + comment, got {events:?}"
        );
        assert_eq!(
            events[0],
            BdEvent::Release {
                repo: repo.to_path_buf(),
                id: "bead-1".to_string(),
            }
        );
        match &events[1] {
            BdEvent::Comment {
                repo: got_repo,
                id,
                text,
            } => {
                assert_eq!(got_repo, repo);
                assert_eq!(id, "bead-1");
                assert!(
                    text.contains(expected_summary),
                    "comment {text:?} did not contain {expected_summary:?}"
                );
            }
            other => panic!("expected comment event, got {other:?}"),
        }
    }

    #[derive(Clone)]
    struct Process {
        status: ProcessStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl Process {
        fn exit(code: i32, stdout: &str, stderr: &str) -> Self {
            Self {
                status: ProcessStatus::code(code),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }
        }
    }

    struct FakeExec {
        processes: RefCell<Vec<Process>>,
        spawns: RefCell<Vec<SpawnRequest>>,
    }

    impl FakeExec {
        fn new(processes: Vec<Process>) -> Self {
            Self {
                processes: RefCell::new(processes),
                spawns: RefCell::new(Vec::new()),
            }
        }

        fn spawns(&self) -> Vec<SpawnRequest> {
            self.spawns.borrow().clone()
        }
    }

    impl Exec for FakeExec {
        fn spawn(&self, request: &SpawnRequest) -> crate::dispatch::Result<Box<dyn ChildProcess>> {
            let process = self.processes.borrow_mut().remove(0);
            if let Some(parent) = request.stdout_path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir stdout parent");
            }
            if let Some(parent) = request.stderr_path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir stderr parent");
            }
            std::fs::write(&request.stdout_path, &process.stdout).expect("write stdout");
            std::fs::write(&request.stderr_path, &process.stderr).expect("write stderr");
            self.spawns.borrow_mut().push(request.clone());
            Ok(Box::new(FakeChild {
                status: process.status,
            }))
        }
    }

    struct FakeChild {
        status: ProcessStatus,
    }

    impl ChildProcess for FakeChild {
        fn wait_for(
            &mut self,
            _timeout: Duration,
        ) -> crate::dispatch::Result<Option<ProcessStatus>> {
            Ok(Some(self.status))
        }

        fn terminate(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> crate::dispatch::Result<ProcessStatus> {
            Ok(self.status)
        }
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
        fn head(&self, _repo: &Path) -> crate::dispatch::Result<Option<String>> {
            Ok(self.heads.borrow_mut().remove(0))
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BdEvent {
        Release {
            repo: PathBuf,
            id: String,
        },
        Close {
            repo: PathBuf,
            id: String,
            reason: String,
        },
        Comment {
            repo: PathBuf,
            id: String,
            text: String,
        },
    }

    struct FakeBdClient {
        issue: Issue,
        events: RefCell<Vec<BdEvent>>,
    }

    impl FakeBdClient {
        fn new(issue: &Issue) -> Self {
            Self {
                issue: issue.clone(),
                events: RefCell::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<BdEvent> {
            self.events.borrow().clone()
        }

        fn close_count(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|e| matches!(e, BdEvent::Close { .. }))
                .count()
        }
    }

    impl BdClient for FakeBdClient {
        fn ready(&self, _repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            Err(BdError::new("ready not implemented in fake"))
        }

        fn show(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("show not implemented in fake"))
        }

        fn count(&self, _repo: &Path) -> crate::bd::Result<u64> {
            Err(BdError::new("count not implemented in fake"))
        }

        fn blocked(&self, _repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            Err(BdError::new("blocked not implemented in fake"))
        }

        fn claim(&self, _repo: &Path, _id: &str, _actor: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("claim not implemented in fake"))
        }

        fn release(&self, repo: &Path, id: &str) -> crate::bd::Result<Issue> {
            self.events.borrow_mut().push(BdEvent::Release {
                repo: repo.to_path_buf(),
                id: id.to_string(),
            });
            let mut issue = self.issue.clone();
            issue.status = "open".to_string();
            issue.assignee = None;
            Ok(issue)
        }

        fn close(&self, repo: &Path, id: &str, reason: &str) -> crate::bd::Result<Issue> {
            self.events.borrow_mut().push(BdEvent::Close {
                repo: repo.to_path_buf(),
                id: id.to_string(),
                reason: reason.to_string(),
            });
            let mut issue = self.issue.clone();
            issue.status = "closed".to_string();
            Ok(issue)
        }

        fn comment(&self, repo: &Path, id: &str, text: &str) -> crate::bd::Result<Comment> {
            self.events.borrow_mut().push(BdEvent::Comment {
                repo: repo.to_path_buf(),
                id: id.to_string(),
                text: text.to_string(),
            });
            Ok(Comment {
                id: "comment-1".to_string(),
                issue_id: id.to_string(),
                text: text.to_string(),
                author: "conductor".to_string(),
                created_at: "2026-07-02T00:00:00Z".to_string(),
                schema_version: Some(1),
            })
        }

        fn set_metadata(
            &self,
            _repo: &Path,
            _id: &str,
            _key: &str,
            _value: &str,
        ) -> crate::bd::Result<Issue> {
            Err(BdError::new("set_metadata not implemented in fake"))
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-verify-{label}-{nanos}"));
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
