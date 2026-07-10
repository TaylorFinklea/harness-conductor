//! Approved cycle dispatch orchestration (`conductor dispatch <cycle-id>`).

#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::bd::{BdClient, Issue};
use crate::bursar::{self, BudgetAction, BudgetDecision, BursarClient};
use crate::config::{Backend, Ceiling, Config, Cost, CostPolicy, RosterEntry, Tier};
use crate::deck::{self, CalloutLevel, LiveUpdate, ReportStatus};
use crate::dispatch::{self, CommitProbe, DispatchRequest, Exec};
use crate::fields::{self, RoutingFields, Triage};
use crate::ledger::{self, LedgerRow};
use crate::plan::CyclePlan;
use crate::triage::{self, CandidateRejection};
use crate::verify::{self, ReviewSettings, VerifyDecision, VerifyRequest};

const WORKER_TEMPLATE: &str = include_str!("../templates/worker-prompt.md");
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DISPATCH_PLAN_BLOCK_ID: &str = "dispatch-plan";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalGate {
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchCycleOptions {
    item_timeout: Duration,
    heartbeat_interval: Duration,
}

impl DispatchCycleOptions {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self {
            item_timeout: Duration::from_secs(u64::from(cfg.budgets.item_wall_clock_mins) * 60),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(heartbeat_interval: Duration) -> Self {
        Self {
            item_timeout: Duration::from_secs(30),
            heartbeat_interval,
        }
    }
}

pub(crate) trait LiveSink {
    fn patch(&self, report_path: &Path, live: &LiveUpdate) -> std::result::Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DeckLiveSink;

impl LiveSink for DeckLiveSink {
    fn patch(&self, report_path: &Path, live: &LiveUpdate) -> std::result::Result<(), String> {
        deck::patch_live(report_path, live).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchCycleResult {
    pub(crate) gate: ApprovalGate,
    pub(crate) dispatched: u64,
    pub(crate) verified: u64,
    pub(crate) failed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchCycleError {
    NotAnswered,
    Message(String),
}

impl DispatchCycleError {
    pub(crate) const fn is_not_answered(&self) -> bool {
        matches!(self, Self::NotAnswered)
    }

    fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl fmt::Display for DispatchCycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnswered => f.write_str("dispatch-plan not yet answered"),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DispatchCycleError {}

struct PlannedItem {
    repo: String,
    issue_id: String,
    model: String,
    verify_cmd: Option<String>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "top-level dispatch seam keeps injected dependencies explicit"
)]
pub(crate) fn run_dispatch_cycle<
    B: BdClient + ?Sized,
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
    L: LiveSink + ?Sized,
    U: BursarClient + ?Sized,
>(
    cfg: &Config,
    bd: &B,
    exec: &E,
    commits: &C,
    reports_home: &Path,
    state_dir: &Path,
    ledger_path: &Path,
    cycle_id: &str,
    options: &DispatchCycleOptions,
    live: &L,
    bursar: &U,
) -> std::result::Result<DispatchCycleResult, DispatchCycleError> {
    let run_dir = deck::report_run_dir(reports_home, cycle_id)
        .map_err(|e| DispatchCycleError::message(format!("report path: {e}")))?;
    let report_path = run_dir.join("report.json");
    let gate = approval_gate(&run_dir)?;

    match gate {
        ApprovalGate::ChangesRequested => {
            patch_status_if_present(&report_path, ReportStatus::Done)?;
            return Ok(DispatchCycleResult {
                gate,
                dispatched: 0,
                verified: 0,
                failed: 0,
            });
        }
        ApprovalGate::Approved => patch_status_if_present(&report_path, ReportStatus::Answered)?,
    }

    let plan = CyclePlan::load(state_dir, cycle_id)
        .map_err(|e| DispatchCycleError::message(format!("plan load: {e}")))?;
    if plan.cycle_id != cycle_id {
        return Err(DispatchCycleError::message(format!(
            "plan cycle id mismatch: expected {cycle_id}, found {}",
            plan.cycle_id
        )));
    }

    let items = planned_items(&plan);
    let cycle_start = Instant::now();
    let mut dispatched = 0_u64;
    let mut verified = 0_u64;
    let mut failed = 0_u64;

    for item in &items {
        let attempt = dispatch_one(
            cfg,
            bd,
            exec,
            commits,
            state_dir,
            ledger_path,
            cycle_id,
            options,
            live,
            &report_path,
            cycle_start,
            item,
            None,
            bursar,
        )?;
        dispatched += attempt.dispatches;
        match attempt.decision {
            VerifyDecision::Passed => verified += 1,
            VerifyDecision::Failed | VerifyDecision::HardError => failed += 1,
        }
    }

    patch_live(
        live,
        &report_path,
        cycle_start,
        format!("complete {cycle_id}: verified {verified}/{dispatched}"),
        Some(1.0),
    )?;
    patch_status_if_present(&report_path, ReportStatus::Done)?;

    Ok(DispatchCycleResult {
        gate,
        dispatched,
        verified,
        failed,
    })
}

fn approval_gate(run_dir: &Path) -> std::result::Result<ApprovalGate, DispatchCycleError> {
    let responses = deck::read_responses(run_dir)
        .map_err(|e| DispatchCycleError::message(format!("responses: {e}")))?;
    let Some(response) = responses.response_after(DISPATCH_PLAN_BLOCK_ID, None) else {
        return Err(DispatchCycleError::NotAnswered);
    };
    match response.value() {
        "approved" => Ok(ApprovalGate::Approved),
        "changes-requested" => Ok(ApprovalGate::ChangesRequested),
        other => Err(DispatchCycleError::message(format!(
            "unsupported dispatch-plan response {other:?}"
        ))),
    }
}

fn planned_items(plan: &CyclePlan) -> Vec<PlannedItem> {
    let mut items = Vec::with_capacity(plan.dispatches.len() + plan.proposals.len());
    items.extend(plan.dispatches.iter().map(|entry| PlannedItem {
        repo: entry.repo.clone(),
        issue_id: entry.issue_id.clone(),
        model: entry.model.clone(),
        verify_cmd: Some(entry.verify_cmd.clone()),
    }));
    items.extend(plan.proposals.iter().map(|entry| PlannedItem {
        repo: entry.repo.clone(),
        issue_id: entry.issue_id.clone(),
        model: entry.model.clone(),
        verify_cmd: None,
    }));
    items
}

struct DispatchOneResult {
    decision: VerifyDecision,
    dispatches: u64,
}

struct WorkerAttempt {
    roster: RosterEntry,
    result: dispatch::DispatchResult,
    attempts: u64,
}

enum WorkerChainOutcome {
    Ran(WorkerAttempt),
    Deferred(String),
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "orchestrates injected M4 seams explicitly"
)]
fn dispatch_one<
    B: BdClient + ?Sized,
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
    L: LiveSink + ?Sized,
    U: BursarClient + ?Sized,
>(
    cfg: &Config,
    bd: &B,
    exec: &E,
    commits: &C,
    state_dir: &Path,
    ledger_path: &Path,
    cycle_id: &str,
    options: &DispatchCycleOptions,
    live: &L,
    report_path: &Path,
    cycle_start: Instant,
    item: &PlannedItem,
    progress: Option<f64>,
    bursar: &U,
) -> std::result::Result<DispatchOneResult, DispatchCycleError> {
    let repo_path = repo_path(cfg, &item.repo)?;
    let roster = cfg
        .roster
        .iter()
        .find(|entry| entry.name == item.model)
        .ok_or_else(|| {
            DispatchCycleError::message(format!("plan references unknown model {}", item.model))
        })?;

    patch_live(
        live,
        report_path,
        cycle_start,
        format!("claim {}/{}", item.repo, item.issue_id),
        progress,
    )?;
    let claimed = bd
        .claim(&repo_path, &item.issue_id, "conductor")
        .map_err(|e| DispatchCycleError::message(format!("bd claim: {e}")))?;
    let extracted = extract_dispatch_fields(&claimed, item.verify_cmd.as_deref()).map_err(|e| {
        let _ = bd.release(&repo_path, &item.issue_id);
        let _ = bd.comment(
            &repo_path,
            &item.issue_id,
            &format!(
                "conductor: {cycle_id} {} dispatch refused: {e}",
                item.issue_id
            ),
        );
        e
    })?;

    let prompt = render_worker_prompt(&claimed, &repo_path, &extracted.verify_cmd);
    let before_head = commits
        .head(&repo_path)
        .map_err(|e| DispatchCycleError::message(format!("git head before worker: {e}")))?;
    let worker_step = format!("worker {}/{}", item.repo, item.issue_id);
    let worker_outcome = run_worker_chain(
        cfg,
        exec,
        commits,
        state_dir,
        ledger_path,
        cycle_id,
        options,
        live,
        report_path,
        cycle_start,
        item,
        roster,
        &claimed,
        &extracted,
        &repo_path,
        &prompt,
        &worker_step,
        progress,
        bursar,
    )
    .map_err(|e| {
        let _ = bd.release(&repo_path, &item.issue_id);
        let _ = bd.comment(
            &repo_path,
            &item.issue_id,
            &format!("conductor: {cycle_id} {} worker failed: {e}", item.issue_id),
        );
        let _ = append_ledger(
            ledger_path,
            roster,
            &item.repo,
            &claimed,
            &extracted,
            "implement",
            false,
            cycle_id,
            &format!("worker spawn failed: {e}"),
        );
        DispatchCycleError::message(format!("dispatch: {e}"))
    })?;
    let worker_attempt = match worker_outcome {
        WorkerChainOutcome::Ran(worker_attempt) => worker_attempt,
        WorkerChainOutcome::Deferred(summary) => {
            let _ = bd.release(&repo_path, &item.issue_id);
            let _ = bd.comment(
                &repo_path,
                &item.issue_id,
                &format!(
                    "conductor: {cycle_id} {} budget deferred: {summary}",
                    item.issue_id
                ),
            );
            append_ledger(
                ledger_path,
                roster,
                &item.repo,
                &claimed,
                &extracted,
                "implement",
                false,
                cycle_id,
                &format!("budget deferred: {summary}"),
            )?;
            return Ok(DispatchOneResult {
                decision: VerifyDecision::Failed,
                dispatches: 0,
            });
        }
    };
    let active_roster = worker_attempt.roster;

    patch_live(
        live,
        report_path,
        cycle_start,
        format!("verify {}/{}", item.repo, item.issue_id),
        progress,
    )?;
    let verify_request = VerifyRequest {
        repo: repo_path,
        state_dir: state_dir.to_path_buf(),
        cycle_id: cycle_id.to_string(),
        issue: claimed.clone(),
        verify_cmd: extracted.verify_cmd.clone(),
        verify: cfg.verify.clone(),
        worker_status: worker_attempt.result.status.clone(),
        before_head,
    };
    let review = ReviewSettings {
        config: cfg.review.clone(),
        roster: cfg.roster.clone(),
        dispatched_model: active_roster.clone(),
        item_tier_floor: extracted.routing.tier_floor,
    };
    let outcome = verify::run_with_review(bd, exec, commits, &verify_request, &review)
        .map_err(|e| DispatchCycleError::message(format!("verify: {e}")))?;
    if let Some(review) = &outcome.review {
        let reviewer = cfg
            .roster
            .iter()
            .find(|entry| entry.name == review.model)
            .ok_or_else(|| {
                DispatchCycleError::message(format!(
                    "review referenced unknown model {}",
                    review.model
                ))
            })?;
        append_ledger(
            ledger_path,
            reviewer,
            &item.repo,
            &claimed,
            &extracted,
            "review",
            review.verify_passed,
            cycle_id,
            &review.summary,
        )?;
    }
    append_ledger(
        ledger_path,
        &active_roster,
        &item.repo,
        &claimed,
        &extracted,
        "implement",
        outcome.verify_passed,
        cycle_id,
        &outcome.summary,
    )?;
    Ok(DispatchOneResult {
        decision: outcome.decision,
        dispatches: worker_attempt.attempts + outcome.review_dispatches,
    })
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "keeps fallback runtime explicit at dispatch boundary"
)]
fn run_worker_chain<E, C, L, U>(
    cfg: &Config,
    exec: &E,
    commits: &C,
    state_dir: &Path,
    ledger_path: &Path,
    cycle_id: &str,
    options: &DispatchCycleOptions,
    live: &L,
    report_path: &Path,
    cycle_start: Instant,
    item: &PlannedItem,
    initial_roster: &RosterEntry,
    issue: &Issue,
    fields: &ExtractedFields,
    repo_path: &Path,
    prompt: &str,
    worker_step: &str,
    progress: Option<f64>,
    bursar_client: &U,
) -> std::result::Result<WorkerChainOutcome, DispatchCycleError>
where
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
    L: LiveSink + ?Sized,
    U: BursarClient + ?Sized,
{
    let chain = fallback_chain(&cfg.roster, initial_roster)?;
    let repo_cost_policy = cfg.cost_policy_for(&item.repo);
    let mut attempts = 0_u64;
    let mut deferred = Vec::new();
    for (idx, roster) in chain.iter().enumerate() {
        if let Some(rejection) =
            triage::candidate_rejection(roster, &fields.routing, repo_cost_policy)
        {
            record_fallback_skip(report_path, item, roster, rejection, fields)?;
            continue;
        }

        if is_metered_worker_backend(roster.backend) {
            let provider = bursar_provider_for(roster);
            let decision =
                bursar::evaluate_budget(bursar_client, &provider, cfg.budgets.use_bursar);
            record_budget_decision(report_path, item, roster, &decision)?;
            if decision.action == BudgetAction::Defer {
                deferred.push(decision.summary.clone());
                let Some(next) =
                    next_eligible_roster(&chain, idx + 1, &fields.routing, repo_cost_policy)
                else {
                    record_remaining_ineligible(
                        &chain,
                        idx + 1,
                        report_path,
                        item,
                        &fields.routing,
                        repo_cost_policy,
                        fields,
                    )?;
                    return Ok(WorkerChainOutcome::Deferred(deferred.join("; ")));
                };
                patch_live(
                    live,
                    report_path,
                    cycle_start,
                    format!(
                        "budget defer {}/{}: {} -> {}",
                        item.repo, item.issue_id, roster.name, next.name
                    ),
                    progress,
                )?;
                continue;
            }
        }

        attempts += 1;
        let request = DispatchRequest {
            repo: repo_path.to_path_buf(),
            cycle_id: cycle_id.to_string(),
            bead_id: item.issue_id.clone(),
            backend: roster.backend,
            dispatch_id: roster.dispatch_id.clone(),
            reasoning_effort: roster.reasoning_effort,
            prompt: prompt.to_string(),
        };
        let result = dispatch::run_with_heartbeat(
            exec,
            commits,
            &request,
            state_dir,
            options.item_timeout,
            options.heartbeat_interval,
            |_elapsed| {
                let bounded = duration_millis_u64(cycle_start.elapsed());
                let live_update = LiveUpdate::new(timestamp())
                    .with_step(worker_step.to_string())
                    .with_elapsed_ms(bounded)
                    .with_progress(progress.unwrap_or(0.0));
                live.patch(report_path, &live_update)
                    .map_err(dispatch::DispatchError::new)
            },
        )
        .map_err(|e| DispatchCycleError::message(e.to_string()))?;

        let Some(reason) = retryable_failure_reason(&result)? else {
            return Ok(WorkerChainOutcome::Ran(WorkerAttempt {
                roster: roster.clone(),
                result,
                attempts,
            }));
        };
        let Some(next) = next_eligible_roster(&chain, idx + 1, &fields.routing, repo_cost_policy)
        else {
            record_remaining_ineligible(
                &chain,
                idx + 1,
                report_path,
                item,
                &fields.routing,
                repo_cost_policy,
                fields,
            )?;
            append_ledger(
                ledger_path,
                roster,
                &item.repo,
                issue,
                fields,
                "implement",
                false,
                cycle_id,
                &format!("retryable worker failure ({reason}); no eligible fallback"),
            )?;
            return Ok(WorkerChainOutcome::Ran(WorkerAttempt {
                roster: roster.clone(),
                result,
                attempts,
            }));
        };
        append_ledger(
            ledger_path,
            roster,
            &item.repo,
            issue,
            fields,
            "implement",
            false,
            cycle_id,
            &format!(
                "retryable worker failure ({reason}); failover to {}",
                next.name
            ),
        )?;
        patch_live(
            live,
            report_path,
            cycle_start,
            format!(
                "failover {}/{}: {} -> {}",
                item.repo, item.issue_id, roster.name, next.name
            ),
            progress,
        )?;
    }
    Err(DispatchCycleError::message(
        "empty eligible worker fallback chain",
    ))
}

fn is_metered_worker_backend(backend: Backend) -> bool {
    matches!(backend, Backend::Pi | Backend::Agy | Backend::Codex)
}

fn bursar_provider_for(roster: &RosterEntry) -> String {
    let raw = if !roster.provider.is_empty() {
        roster.provider.as_str()
    } else if roster.backend == Backend::Agy {
        "agy"
    } else {
        roster
            .dispatch_id
            .split_once('/')
            .map_or(roster.dispatch_id.as_str(), |(provider, _)| provider)
    };
    match raw {
        "openai-codex" => "codex".to_string(),
        other => other.to_string(),
    }
}

fn record_budget_decision(
    report_path: &Path,
    item: &PlannedItem,
    roster: &RosterEntry,
    decision: &BudgetDecision,
) -> std::result::Result<(), DispatchCycleError> {
    if !report_path.exists() {
        return Ok(());
    }
    let level = match decision.action {
        BudgetAction::Proceed | BudgetAction::StaticCaps => CalloutLevel::Info,
        BudgetAction::SpendCautiously | BudgetAction::Defer => CalloutLevel::Warn,
    };
    deck::append_callout(
        report_path,
        level,
        "BURSAR",
        &format!(
            "bursar budget decision: {}/{} → {} ({})\n- model: {}\n- {}",
            item.repo,
            item.issue_id,
            decision.action.label(),
            decision.provider,
            roster.name,
            decision.summary
        ),
    )
    .map_err(|e| DispatchCycleError::message(format!("report budget decision: {e}")))
}

fn next_eligible_roster<'a>(
    chain: &'a [RosterEntry],
    start: usize,
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
) -> Option<&'a RosterEntry> {
    chain
        .iter()
        .skip(start)
        .find(|roster| triage::candidate_rejection(roster, routing, repo_cost_policy).is_none())
}

fn record_remaining_ineligible(
    chain: &[RosterEntry],
    start: usize,
    report_path: &Path,
    item: &PlannedItem,
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
    fields: &ExtractedFields,
) -> std::result::Result<(), DispatchCycleError> {
    for roster in chain.iter().skip(start) {
        if let Some(rejection) = triage::candidate_rejection(roster, routing, repo_cost_policy) {
            record_fallback_skip(report_path, item, roster, rejection, fields)?;
        }
    }
    Ok(())
}

fn record_fallback_skip(
    report_path: &Path,
    item: &PlannedItem,
    roster: &RosterEntry,
    rejection: CandidateRejection,
    fields: &ExtractedFields,
) -> std::result::Result<(), DispatchCycleError> {
    if !report_path.exists() {
        return Ok(());
    }
    let mut note = serde_json::json!({
        "event": "fallback_skip",
        "repo": item.repo,
        "issue_id": item.issue_id,
        "model": roster.name,
        "tier_floor": tier_label(fields.routing.tier_floor),
        "complexity": ceiling_label(fields.routing.complexity),
        "data_policy_trains_ok": fields.routing.trains_ok,
    });
    if let Some(object) = note.as_object_mut() {
        match rejection {
            CandidateRejection::BelowTierFloor { required, actual } => {
                object.insert("reason".to_string(), serde_json::json!("below-tier-floor"));
                object.insert(
                    "required_tier".to_string(),
                    serde_json::json!(tier_label(required)),
                );
                object.insert(
                    "actual_tier".to_string(),
                    serde_json::json!(tier_label(actual)),
                );
            }
            CandidateRejection::BelowCeiling { required, actual } => {
                object.insert("reason".to_string(), serde_json::json!("below-ceiling"));
                object.insert(
                    "required_ceiling".to_string(),
                    serde_json::json!(ceiling_label(required)),
                );
                object.insert(
                    "actual_ceiling".to_string(),
                    serde_json::json!(ceiling_label(actual)),
                );
            }
            CandidateRejection::CostPolicy { policy, cost } => {
                object.insert("reason".to_string(), serde_json::json!("cost-policy"));
                object.insert(
                    "repo_cost_policy".to_string(),
                    serde_json::json!(cost_policy_label(policy)),
                );
                object.insert(
                    "model_cost".to_string(),
                    serde_json::json!(cost_label(cost)),
                );
            }
        }
    }
    deck::append_callout(
        report_path,
        CalloutLevel::Warn,
        "FALLBACK_SKIP",
        &note.to_string(),
    )
    .map_err(|e| DispatchCycleError::message(format!("report fallback skip: {e}")))
}

fn fallback_chain(
    roster: &[RosterEntry],
    initial: &RosterEntry,
) -> std::result::Result<Vec<RosterEntry>, DispatchCycleError> {
    let mut chain = Vec::with_capacity(1 + initial.fallback.len());
    chain.push(initial.clone());
    for name in &initial.fallback {
        let entry = roster
            .iter()
            .find(|entry| entry.name == *name)
            .ok_or_else(|| {
                DispatchCycleError::message(format!(
                    "roster entry {} fallback references unknown model {name}",
                    initial.name
                ))
            })?;
        chain.push(entry.clone());
    }
    Ok(chain)
}

fn retryable_failure_reason(
    result: &dispatch::DispatchResult,
) -> std::result::Result<Option<String>, DispatchCycleError> {
    if !matches!(result.status, dispatch::DispatchStatus::Failed(_)) {
        return Ok(None);
    }
    let stderr = std::fs::read_to_string(&result.stderr_path).map_err(|e| {
        DispatchCycleError::message(format!(
            "read worker stderr {}: {e}",
            result.stderr_path.display()
        ))
    })?;
    if !is_retryable_worker_stderr(&stderr) {
        return Ok(None);
    }
    let reason = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("retryable provider failure")
        .trim()
        .to_string();
    Ok(Some(reason))
}

fn is_retryable_worker_stderr(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    contains_contextual_429(&stderr)
        || stderr.contains("quota")
        || stderr.contains("rate_limit")
        || stderr.contains("rate limit")
        || stderr.contains("too many requests")
}

fn contains_contextual_429(stderr: &str) -> bool {
    let normalized: String = stderr
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    tokens.iter().enumerate().any(|(idx, token)| {
        if *token != "429" {
            return false;
        }
        let previous = idx.checked_sub(1).and_then(|i| tokens.get(i).copied());
        let previous_two = idx.checked_sub(2).and_then(|i| tokens.get(i).copied());
        matches!(
            previous,
            Some("http" | "https" | "status" | "code" | "response")
        ) || matches!(
            (previous_two, previous),
            (Some("status" | "http"), Some("code" | "status"))
        )
    })
}

struct ExtractedFields {
    verify_cmd: String,
    routing: RoutingFields,
}

fn extract_dispatch_fields(
    issue: &Issue,
    planned_verify_cmd: Option<&str>,
) -> std::result::Result<ExtractedFields, DispatchCycleError> {
    let routing = match fields::extract(issue) {
        Triage::Triaged(routing) => routing,
        Triage::Untriaged { missing } => {
            return Err(DispatchCycleError::message(format!(
                "issue {} is untriaged: {missing:?}",
                issue.id
            )));
        }
    };
    let verify_cmd = planned_verify_cmd
        .map(str::to_string)
        .or_else(|| routing.verify_cmd.clone())
        .ok_or_else(|| {
            DispatchCycleError::message(format!("issue {} has no verify_cmd", issue.id))
        })?;
    Ok(ExtractedFields {
        verify_cmd,
        routing,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "ledger rows mirror the external JSONL shape"
)]
fn append_ledger(
    ledger_path: &Path,
    roster: &RosterEntry,
    repo: &str,
    issue: &Issue,
    fields: &ExtractedFields,
    role: &str,
    verify_passed: bool,
    cycle_id: &str,
    summary: &str,
) -> std::result::Result<(), DispatchCycleError> {
    let row = LedgerRow {
        date: Utc::now().format("%Y-%m-%d").to_string(),
        model: roster.name.clone(),
        harness: None,
        profile: None,
        reasoning_effort: roster
            .reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        role: role.to_string(),
        task: issue.id.clone(),
        score_1_5: None,
        blind_rank: None,
        judge: None,
        verify_passed,
        complexity: ceiling_label(fields.routing.complexity).to_string(),
        project: repo.to_string(),
        bias_note: None,
        notes: format!("conductor {cycle_id}: {summary}"),
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
    ledger::append(ledger_path, &row)
        .map_err(|e| DispatchCycleError::message(format!("ledger: {e}")))
}

fn render_worker_prompt(issue: &Issue, repo: &Path, verify_cmd: &str) -> String {
    let repo = repo.display().to_string();
    let mut out = String::with_capacity(WORKER_TEMPLATE.len() + issue.description.len());
    let mut rest = WORKER_TEMPLATE;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after_open[..end];
        if !append_placeholder(&mut out, key, issue, &repo, verify_cmd) {
            out.push_str("{{");
            out.push_str(key);
            out.push_str("}}");
        }
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    out
}

fn append_placeholder(
    out: &mut String,
    key: &str,
    issue: &Issue,
    repo: &str,
    verify_cmd: &str,
) -> bool {
    match key {
        "bead_id" => out.push_str(&issue.id),
        "title" => out.push_str(&issue.title),
        "description" => out.push_str(&issue.description),
        "acceptance" => out.push_str(&issue.acceptance_criteria),
        "notes" => out.push_str(&issue.notes),
        "repo" => out.push_str(repo),
        "verify_cmd" => out.push_str(verify_cmd),
        _ => return false,
    }
    true
}

fn patch_live<L: LiveSink + ?Sized>(
    live: &L,
    report_path: &Path,
    cycle_start: Instant,
    step: String,
    progress: Option<f64>,
) -> std::result::Result<(), DispatchCycleError> {
    let elapsed_ms = duration_millis_u64(cycle_start.elapsed());
    let mut update = LiveUpdate::new(timestamp())
        .with_step(step)
        .with_elapsed_ms(elapsed_ms);
    if let Some(progress) = progress {
        update = update.with_progress(progress);
    }
    live.patch(report_path, &update)
        .map_err(|e| DispatchCycleError::message(format!("live patch: {e}")))
}

fn patch_status_if_present(
    report_path: &Path,
    status: ReportStatus,
) -> std::result::Result<(), DispatchCycleError> {
    if !report_path.exists() {
        return Ok(());
    }
    deck::patch_status(report_path, status)
        .map_err(|e| DispatchCycleError::message(format!("report status: {e}")))
}

fn repo_path(cfg: &Config, repo: &str) -> std::result::Result<PathBuf, DispatchCycleError> {
    let repo_path = PathBuf::from(repo);
    if repo_path.is_absolute() {
        return Ok(repo_path);
    }
    Ok(expand_tilde(&cfg.scan.root)?.join(repo))
}

fn expand_tilde(path: &str) -> std::result::Result<PathBuf, DispatchCycleError> {
    if !path.starts_with('~') {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .map_err(|_| DispatchCycleError::message("HOME not set; cannot expand ~"))?;
    if home.is_empty() {
        return Err(DispatchCycleError::message(
            "HOME is empty; cannot expand ~",
        ));
    }
    let rest = path.strip_prefix("~/").unwrap_or(&path[1..]);
    Ok(PathBuf::from(home).join(rest))
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Lead => "lead",
        Tier::Senior => "senior",
        Tier::Junior => "junior",
    }
}

fn ceiling_label(ceiling: Ceiling) -> &'static str {
    match ceiling {
        Ceiling::S => "S",
        Ceiling::M => "M",
        Ceiling::L => "L",
        Ceiling::Xl => "XL",
    }
}

fn cost_label(cost: Cost) -> &'static str {
    match cost {
        Cost::Paid => "paid",
        Cost::Free => "free",
        Cost::FreeTrainsInput => "free-trains-input",
    }
}

fn cost_policy_label(policy: CostPolicy) -> &'static str {
    match policy {
        CostPolicy::Proprietary => "proprietary",
        CostPolicy::Internal => "internal",
        CostPolicy::Oss => "oss",
        CostPolicy::Public => "public",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::bd::{BdClient, BdError, CommandBdClient, Comment, Issue};
    use crate::bursar::{test_support::FakeBursarClient, ProviderState};
    use crate::config;
    use crate::deck::{Block, Report, ReportStatus};
    use crate::dispatch::{
        ChildProcess, CommitProbe, Exec, GitCommitProbe, ProcessStatus, SpawnRequest,
    };
    use crate::plan::{CyclePlan, ProposalEntry};

    #[test]
    fn approval_gate_matrix_refuses_absent_closes_changes_requested_and_runs_approved() {
        let temp = TempDir::new("approval-gate");
        let cfg = config::parse_str(fixture_config(temp.path())).expect("config parses");
        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("model-bench.jsonl");
        write_empty_plan(&state, "cycle-approved");
        write_empty_plan(&state, "cycle-changes");
        write_empty_plan(&state, "cycle-absent");
        write_report(&reports, "cycle-approved");
        write_report(&reports, "cycle-changes");
        write_report(&reports, "cycle-absent");
        write_response(&reports, "cycle-approved", "approved");
        write_response(&reports, "cycle-changes", "changes-requested");

        let bd = PanicBdClient;
        let exec = PanicExec;
        let commits = PanicCommits;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));
        let bursar = FakeBursarClient::unavailable();

        let approved = run_dispatch_cycle(
            &cfg,
            &bd,
            &exec,
            &commits,
            &reports,
            &state,
            &ledger,
            "cycle-approved",
            &options,
            &live,
            &bursar,
        )
        .expect("approved empty plan succeeds");
        assert_eq!(approved.gate, ApprovalGate::Approved);
        assert_eq!(approved.dispatched, 0);

        let changes = run_dispatch_cycle(
            &cfg,
            &bd,
            &exec,
            &commits,
            &reports,
            &state,
            &ledger,
            "cycle-changes",
            &options,
            &live,
            &bursar,
        )
        .expect("changes-requested closes without running");
        assert_eq!(changes.gate, ApprovalGate::ChangesRequested);
        assert_eq!(changes.dispatched, 0);
        let report_path = report_path(&reports, "cycle-changes");
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "done");

        let absent = run_dispatch_cycle(
            &cfg,
            &bd,
            &exec,
            &commits,
            &reports,
            &state,
            &ledger,
            "cycle-absent",
            &options,
            &live,
            &bursar,
        )
        .expect_err("missing approval refuses");
        assert!(absent.is_not_answered());
        assert!(absent
            .to_string()
            .contains("dispatch-plan not yet answered"));
    }

    #[test]
    fn e2e_sandbox() {
        let temp = TempDir::new("e2e-sandbox");
        let fleet = temp.path().join("fleet");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        let repo = fleet.join("sandbox-repo");
        init_sandbox_repo(&repo);
        create_sandbox_bead(&repo);

        let cfg = config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 4
item_wall_clock_mins = 1
cycle_wall_clock_mins = 1

[verify]
judge = "opencode-go/qwen3.7-max"
always_orchestra = false

[review]
enabled = false
min_tier_gap = 1

[[roster]]
name = "fake-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "fake-worker"
"#,
            fleet.display()
        ))
        .expect("config parses");

        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("ledger").join("model-bench.jsonl");
        let cycle_id = "cycle-20260702-010203";
        write_plan_with_proposal(&state, cycle_id, "sandbox-repo", "sandbox-1", "fake-worker");
        write_report(&reports, cycle_id);
        write_response(&reports, cycle_id, "approved");

        let bd = CommandBdClient::new();
        let exec = SandboxExec::new();
        let commits = GitCommitProbe;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));
        let bursar = FakeBursarClient::unavailable();

        let result = run_dispatch_cycle(
            &cfg, &bd, &exec, &commits, &reports, &state, &ledger, cycle_id, &options, &live,
            &bursar,
        )
        .expect("approved sandbox dispatch succeeds");

        assert_eq!(result.gate, ApprovalGate::Approved);
        assert_eq!(result.dispatched, 1);
        assert_eq!(result.verified, 1);

        let issue = bd.show(&repo, "sandbox-1").expect("show closed issue");
        assert_eq!(issue.status, "closed");
        assert_eq!(issue.assignee.as_deref(), Some("conductor"));

        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 2, "worker + verify_cmd");
        let worker_prompt = prompt_arg(&spawns[0]);
        assert!(worker_prompt.contains("sandbox-1"));
        assert!(worker_prompt.contains("Synthetic sandbox bead"));
        assert!(worker_prompt.contains("sandbox description"));
        assert!(worker_prompt.contains("worker.txt exists"));
        assert!(worker_prompt.contains("tier_floor: junior"));
        assert!(worker_prompt.contains(&repo.display().to_string()));
        assert!(worker_prompt.contains("test -f worker.txt"));
        assert_eq!(spawns[0].cwd, repo);

        let head = git(&fleet.join("sandbox-repo"), &["log", "--oneline", "-1"]);
        assert!(head.contains("worker: complete sandbox bead"));

        let ledger_line = std::fs::read_to_string(&ledger).expect("ledger exists");
        let rows: Vec<serde_json::Value> = ledger_line
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger line json"))
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["model"], "fake-worker");
        assert_eq!(rows[0]["role"], "implement");
        assert_eq!(rows[0]["task"], "sandbox-1");
        assert_eq!(rows[0]["verify_passed"], true);
        assert_eq!(rows[0]["complexity"], "S");
        assert_eq!(rows[0]["project"], "sandbox-repo");
        assert!(rows[0].get("score_1_5").is_none());

        let heartbeats = live.updates();
        assert!(
            heartbeats.len() >= 2,
            "expected multiple live patches, got {heartbeats:?}"
        );
        assert!(heartbeats
            .iter()
            .any(|step| step.contains("worker sandbox-repo/sandbox-1")));
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path(&reports, cycle_id)).unwrap())
                .unwrap();
        assert_eq!(report["status"], "done");
        assert!(report["live"]["step"]
            .as_str()
            .unwrap()
            .contains("complete"));
    }

    #[test]
    fn review_e2e_sandbox_junior_tier_dispatch_gets_senior_review_and_counts_budget() {
        let temp = TempDir::new("review-e2e-sandbox");
        let fleet = temp.path().join("fleet");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        let repo = fleet.join("sandbox-repo");
        init_sandbox_repo_without_bd(&repo);

        let cfg = config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 8
item_wall_clock_mins = 1
cycle_wall_clock_mins = 1

[verify]
judge = "opencode-go/qwen3.7-max"
always_orchestra = false

[review]
enabled = true
min_tier_gap = 1

[[roster]]
name = "fake-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "fake-worker"

[[roster]]
name = "senior-reviewer"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "senior-reviewer"
"#,
            fleet.display()
        ))
        .expect("config parses");

        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("ledger").join("model-bench.jsonl");
        let cycle_id = "cycle-20260702-review";
        write_plan_with_proposal(&state, cycle_id, "sandbox-repo", "sandbox-1", "fake-worker");
        write_report(&reports, cycle_id);
        write_response(&reports, cycle_id, "approved");

        let bd = RecordingBdClient::new(sandbox_issue());
        let exec = SandboxExec::new();
        let commits = GitCommitProbe;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));
        let bursar = FakeBursarClient::unavailable();

        let result = run_dispatch_cycle(
            &cfg, &bd, &exec, &commits, &reports, &state, &ledger, cycle_id, &options, &live,
            &bursar,
        )
        .expect("approved sandbox dispatch with review succeeds");

        assert_eq!(result.gate, ApprovalGate::Approved);
        assert_eq!(
            result.dispatched, 2,
            "worker + review dispatch are budget-counted"
        );
        assert_eq!(result.verified, 1);
        assert_eq!(bd.close_count(), 1);

        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 3, "worker + verify_cmd + review");
        assert!(prompt_arg(&spawns[2]).contains("READ-ONLY qualitative review"));
        assert!(spawns[2].argv.contains(&"senior-reviewer".to_string()));

        let ledger_line = std::fs::read_to_string(&ledger).expect("ledger exists");
        let rows: Vec<serde_json::Value> = ledger_line
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger line json"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["role"], "review");
        assert_eq!(rows[0]["model"], "senior-reviewer");
        assert_eq!(rows[1]["role"], "implement");
        assert_eq!(rows[1]["model"], "fake-worker");
    }

    #[test]
    fn fallback_e2e_retries_retryable_worker_failure_and_verifies_fallback_commit() {
        let temp = TempDir::new("fallback-e2e-sandbox");
        let fleet = temp.path().join("fleet");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        let repo = fleet.join("sandbox-repo");
        init_sandbox_repo_without_bd(&repo);

        let cfg = config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 8
item_wall_clock_mins = 1
cycle_wall_clock_mins = 1

[verify]
judge = "opencode-go/qwen3.7-max"
always_orchestra = false

[review]
enabled = false
min_tier_gap = 1

[[roster]]
name = "primary-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "primary-worker"
fallback = ["fallback-worker"]

[[roster]]
name = "fallback-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "fallback-worker"
"#,
            fleet.display()
        ))
        .expect("config parses");

        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("ledger").join("model-bench.jsonl");
        let cycle_id = "cycle-20260706-fallback";
        write_plan_with_proposal(
            &state,
            cycle_id,
            "sandbox-repo",
            "sandbox-1",
            "primary-worker",
        );
        write_report(&reports, cycle_id);
        write_response(&reports, cycle_id, "approved");

        let bd = RecordingBdClient::new(sandbox_issue());
        let exec = FallbackExec::new();
        let commits = GitCommitProbe;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));
        let bursar = FakeBursarClient::unavailable();

        let result = run_dispatch_cycle(
            &cfg, &bd, &exec, &commits, &reports, &state, &ledger, cycle_id, &options, &live,
            &bursar,
        )
        .expect("approved fallback dispatch succeeds");

        assert_eq!(result.gate, ApprovalGate::Approved);
        assert_eq!(result.dispatched, 2, "primary attempt + fallback attempt");
        assert_eq!(result.verified, 1);
        assert_eq!(bd.close_count(), 1);

        let spawns = exec.spawns();
        assert_eq!(spawns.len(), 3, "primary worker + fallback worker + verify");
        assert!(spawns[0].argv.contains(&"primary-worker".to_string()));
        assert!(spawns[1].argv.contains(&"fallback-worker".to_string()));
        assert_eq!(spawns[1].cwd, repo);

        let ledger_line = std::fs::read_to_string(&ledger).expect("ledger exists");
        let rows: Vec<serde_json::Value> = ledger_line
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger line json"))
            .collect();
        assert_eq!(rows.len(), 2, "failover row + final implement row");
        assert_eq!(rows[0]["model"], "primary-worker");
        assert_eq!(rows[0]["verify_passed"], false);
        assert!(rows[0]["notes"]
            .as_str()
            .expect("notes")
            .contains("failover to fallback-worker"));
        assert_eq!(rows[1]["model"], "fallback-worker");
        assert_eq!(rows[1]["verify_passed"], true);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end fallback eligibility fixture keeps its config inline"
    )]
    fn fallback_skips_ineligible_failover_targets_with_report_notes() {
        let temp = TempDir::new("fallback-eligibility-sandbox");
        let fleet = temp.path().join("fleet");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        let repo = fleet.join("sandbox-repo");
        init_sandbox_repo_without_bd(&repo);

        let cfg = config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 8
item_wall_clock_mins = 1
cycle_wall_clock_mins = 1

[verify]
judge = "opencode-go/qwen3.7-max"
always_orchestra = false

[review]
enabled = false
min_tier_gap = 1

[[roster]]
name = "primary-worker"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "primary-worker"
fallback = ["below-floor-worker", "below-ceiling-worker", "free-train-worker", "fallback-worker"]

[[roster]]
name = "below-floor-worker"
tier = "junior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "below-floor-worker"

[[roster]]
name = "below-ceiling-worker"
tier = "senior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "below-ceiling-worker"

[[roster]]
name = "free-train-worker"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "free-train-worker"
cost = "free-trains-input"

[[roster]]
name = "fallback-worker"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "fallback-worker"
"#,
            fleet.display()
        ))
        .expect("config parses");

        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("ledger").join("model-bench.jsonl");
        let cycle_id = "cycle-20260707-fallback-eligibility";
        write_plan_with_proposal(
            &state,
            cycle_id,
            "sandbox-repo",
            "sandbox-1",
            "primary-worker",
        );
        write_report(&reports, cycle_id);
        write_response(&reports, cycle_id, "approved");

        let mut issue = sandbox_issue();
        let metadata = issue.metadata.as_mut().expect("metadata");
        metadata.insert("tier_floor".to_string(), json!("senior"));
        metadata.insert("complexity".to_string(), json!("M"));
        let bd = RecordingBdClient::new(issue);
        let exec = FallbackExec::new();
        let commits = GitCommitProbe;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));
        let bursar = FakeBursarClient::unavailable();

        let result = run_dispatch_cycle(
            &cfg, &bd, &exec, &commits, &reports, &state, &ledger, cycle_id, &options, &live,
            &bursar,
        )
        .expect("approved fallback dispatch succeeds after skipping ineligible entries");

        assert_eq!(result.dispatched, 2, "primary attempt + eligible fallback");
        assert_eq!(result.verified, 1);

        let spawns = exec.spawns();
        assert_eq!(
            spawns.len(),
            3,
            "primary worker + eligible fallback + verify"
        );
        assert!(spawns[0].argv.contains(&"primary-worker".to_string()));
        assert!(spawns[1].argv.contains(&"fallback-worker".to_string()));
        assert!(!spawns
            .iter()
            .any(|spawn| spawn.argv.contains(&"below-floor-worker".to_string())));
        assert!(!spawns
            .iter()
            .any(|spawn| spawn.argv.contains(&"below-ceiling-worker".to_string())));
        assert!(!spawns
            .iter()
            .any(|spawn| spawn.argv.contains(&"free-train-worker".to_string())));

        let ledger_line = std::fs::read_to_string(&ledger).expect("ledger exists");
        assert!(ledger_line.contains("failover to fallback-worker"));

        let report = std::fs::read_to_string(report_path(&reports, cycle_id)).expect("report json");
        assert!(report.contains("fallback_skip"));
        assert!(report.contains("below-tier-floor"));
        assert!(report.contains("below-ceiling"));
        assert!(report.contains("cost-policy"));
        assert!(report.contains("free-train-worker"));
    }

    #[test]
    fn retryable_worker_stderr_classifier_matches_provider_limit_signals() {
        assert!(is_retryable_worker_stderr("HTTP 429: too many requests"));
        assert!(is_retryable_worker_stderr("status code: 429"));
        assert!(is_retryable_worker_stderr("quota exceeded"));
        assert!(is_retryable_worker_stderr("provider returned rate_limit"));
        assert!(is_retryable_worker_stderr("provider returned rate limit"));
        assert!(!is_retryable_worker_stderr("panicked at src/foo.rs:429:10"));
        assert!(!is_retryable_worker_stderr("syntax error in worker prompt"));
    }

    #[test]
    fn bursar_budget_healthy_provider_proceeds_and_reports_decision() {
        let run = run_bursar_budget_case(
            "healthy",
            &FakeBursarClient::with_provider_status("opencode-go", ProviderState::Ok, Some(42.0)),
        );

        assert_eq!(run.result.dispatched, 1);
        assert_eq!(run.result.verified, 1);
        assert_eq!(run.exec.spawns().len(), 2, "worker + verify");
        let report = report_json_string(&run.reports, &run.cycle_id);
        assert!(report.contains("bursar budget decision"));
        assert!(report.contains("opencode-go"));
        assert!(report.contains("proceed"));
    }

    #[test]
    fn bursar_budget_unknown_provider_spends_cautiously_and_reports_decision() {
        let run = run_bursar_budget_case(
            "unknown",
            &FakeBursarClient::with_provider_status("opencode-go", ProviderState::Unknown, None),
        );

        assert_eq!(run.result.dispatched, 1);
        assert_eq!(run.result.verified, 1);
        assert_eq!(
            run.exec.spawns().len(),
            2,
            "unknown still spends cautiously"
        );
        let report = report_json_string(&run.reports, &run.cycle_id);
        assert!(report.contains("spend-cautiously"));
        assert!(report.contains("opencode-go"));
    }

    #[test]
    fn bursar_budget_near_exhausted_provider_defers_and_reports_decision() {
        let run = run_bursar_budget_case(
            "near-exhausted",
            &FakeBursarClient::with_provider_status("opencode-go", ProviderState::Ok, Some(96.0)),
        );

        assert_eq!(run.result.dispatched, 0);
        assert_eq!(run.result.verified, 0);
        assert_eq!(run.result.failed, 1);
        assert!(run.exec.spawns().is_empty(), "deferred before worker spawn");
        assert_eq!(run.bd.release_count(), 1, "deferred bead is released");
        let report = report_json_string(&run.reports, &run.cycle_id);
        assert!(report.contains("defer"));
        assert!(report.contains("96.0%"));
    }

    #[test]
    fn bursar_budget_absent_binary_falls_back_to_static_caps_cleanly() {
        let run = run_bursar_budget_case("absent", &FakeBursarClient::unavailable());

        assert_eq!(run.result.dispatched, 1);
        assert_eq!(run.result.verified, 1);
        assert_eq!(
            run.exec.spawns().len(),
            2,
            "worker + verify under static caps"
        );
        let report = report_json_string(&run.reports, &run.cycle_id);
        assert!(report.contains("static-caps"));
        assert!(report.contains("bursar unavailable"));
    }

    struct BursarBudgetRun {
        _temp: TempDir,
        reports: PathBuf,
        cycle_id: String,
        result: DispatchCycleResult,
        bd: RecordingBdClient,
        exec: SandboxExec,
    }

    fn run_bursar_budget_case(label: &str, bursar: &FakeBursarClient) -> BursarBudgetRun {
        let temp = TempDir::new(&format!("bursar-budget-{label}"));
        let fleet = temp.path().join("fleet");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        let repo = fleet.join("sandbox-repo");
        init_sandbox_repo_without_bd(&repo);

        let cfg = config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 8
item_wall_clock_mins = 1
cycle_wall_clock_mins = 1

[verify]
judge = "opencode-go/qwen3.7-max"
always_orchestra = false

[review]
enabled = false
min_tier_gap = 1

[[roster]]
name = "fake-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "opencode-go/fake-worker"
provider = "opencode-go"
"#,
            fleet.display()
        ))
        .expect("config parses");

        let state = temp.path().join("state");
        let reports = temp.path().join("reports");
        let ledger = temp.path().join("ledger").join("model-bench.jsonl");
        let cycle_id = format!("cycle-20260707-bursar-{label}");
        write_plan_with_proposal(
            &state,
            &cycle_id,
            "sandbox-repo",
            "sandbox-1",
            "fake-worker",
        );
        write_report(&reports, &cycle_id);
        write_response(&reports, &cycle_id, "approved");

        let bd = RecordingBdClient::new(sandbox_issue());
        let exec = SandboxExec::new();
        let commits = GitCommitProbe;
        let live = RecordingLiveSink::new(true);
        let options = DispatchCycleOptions::for_tests(Duration::from_millis(1));

        let result = run_dispatch_cycle(
            &cfg, &bd, &exec, &commits, &reports, &state, &ledger, &cycle_id, &options, &live,
            bursar,
        )
        .expect("approved bursar budget dispatch runs");

        BursarBudgetRun {
            _temp: temp,
            reports,
            cycle_id,
            result,
            bd,
            exec,
        }
    }

    fn report_json_string(reports: &Path, cycle_id: &str) -> String {
        std::fs::read_to_string(report_path(reports, cycle_id)).expect("report json")
    }

    fn fixture_config(root: &Path) -> &str {
        Box::leak(
            format!(
                r#"[scan]
root = "{}"

[[roster]]
name = "fake-worker"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "pi"
dispatch_id = "fake-worker"
"#,
                root.display()
            )
            .into_boxed_str(),
        )
    }

    fn write_empty_plan(state: &Path, cycle_id: &str) {
        let plan = CyclePlan {
            cycle_id: cycle_id.to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            dispatches: Vec::new(),
            proposals: Vec::new(),
            flags: Vec::new(),
            skips: Vec::new(),
        };
        plan.save(state).expect("save plan");
    }

    fn write_plan_with_proposal(
        state: &Path,
        cycle_id: &str,
        repo: &str,
        issue_id: &str,
        model: &str,
    ) {
        let plan = CyclePlan {
            cycle_id: cycle_id.to_string(),
            created_at: "2026-07-02T01:02:03Z".to_string(),
            dispatches: Vec::new(),
            proposals: vec![ProposalEntry {
                repo: repo.to_string(),
                issue_id: issue_id.to_string(),
                model: model.to_string(),
            }],
            flags: Vec::new(),
            skips: Vec::new(),
        };
        plan.save(state).expect("save plan");
    }

    fn write_report(reports: &Path, cycle_id: &str) {
        let report = Report::new(
            cycle_id,
            format!("Dispatch {cycle_id}"),
            "2026-07-02T00:00:00Z",
            ReportStatus::AwaitingReview,
            vec![Block::approval("dispatch-plan", "Approve dispatch?")],
        )
        .expect("report");
        crate::deck::write_report(reports, &report).expect("write report");
    }

    fn write_response(reports: &Path, cycle_id: &str, value: &str) {
        let run_dir = reports.join(".harness/reports/conductor").join(cycle_id);
        std::fs::write(
            run_dir.join("responses.json"),
            serde_json::to_vec_pretty(&json!({
                "responses": {
                    "dispatch-plan": {
                        "block": "dispatch-plan",
                        "value": value,
                        "note": "test",
                        "at": "2026-07-02T00:00:01Z"
                    }
                }
            }))
            .expect("responses json"),
        )
        .expect("write responses");
    }

    fn report_path(reports: &Path, cycle_id: &str) -> PathBuf {
        reports
            .join(".harness/reports/conductor")
            .join(cycle_id)
            .join("report.json")
    }

    fn init_sandbox_repo(repo: &Path) {
        init_sandbox_repo_without_bd(repo);
        run(repo, "bd", &["init", "--non-interactive", "-p", "sandbox"]);
    }

    fn init_sandbox_repo_without_bd(repo: &Path) {
        std::fs::create_dir_all(repo).expect("mkdir repo");
        run(repo, "git", &["init", "-b", "main"]);
        run(
            repo,
            "git",
            &["config", "user.email", "conductor@example.test"],
        );
        run(repo, "git", &["config", "user.name", "Conductor Test"]);
        std::fs::write(repo.join("README.md"), "sandbox\n").expect("write readme");
        run(repo, "git", &["add", "README.md"]);
        run(repo, "git", &["commit", "-m", "initial"]);
    }

    fn create_sandbox_bead(repo: &Path) {
        run(
            repo,
            "bd",
            &[
                "create",
                "Synthetic sandbox bead",
                "--id",
                "sandbox-1",
                "--description",
                "sandbox description",
                "--acceptance",
                "worker.txt exists",
                "--notes",
                "tier_floor: junior · complexity: S · verify_type: file",
                "-t",
                "task",
                "-p",
                "1",
                "--metadata",
                r#"{"tier_floor":"junior","complexity":"S","verify_cmd":"test -f worker.txt"}"#,
            ],
        );
    }

    fn sandbox_issue() -> Issue {
        let mut metadata = BTreeMap::new();
        metadata.insert("tier_floor".to_string(), json!("junior"));
        metadata.insert("complexity".to_string(), json!("S"));
        metadata.insert("verify_cmd".to_string(), json!("test -f worker.txt"));
        Issue {
            id: "sandbox-1".to_string(),
            title: "Synthetic sandbox bead".to_string(),
            description: "sandbox description".to_string(),
            acceptance_criteria: "worker.txt exists".to_string(),
            notes: "tier_floor: junior · complexity: S · verify_type: file".to_string(),
            status: "open".to_string(),
            priority: 1,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "test".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: "2026-07-02T00:00:00Z".to_string(),
            started_at: None,
            labels: None,
            estimated_minutes: None,
            metadata: Some(metadata),
            parent: None,
            dependencies: None,
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

    fn run(cwd: &Path, program: &str, args: &[&str]) {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn prompt_arg(spawn: &SpawnRequest) -> &str {
        let pos = spawn
            .argv
            .iter()
            .position(|arg| arg == "-p")
            .expect("-p arg");
        &spawn.argv[pos + 1]
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

    struct RecordingLiveSink {
        patch_report: bool,
        updates: RefCell<Vec<String>>,
    }

    impl RecordingLiveSink {
        fn new(patch_report: bool) -> Self {
            Self {
                patch_report,
                updates: RefCell::new(Vec::new()),
            }
        }

        fn updates(&self) -> Vec<String> {
            self.updates.borrow().clone()
        }
    }

    impl LiveSink for RecordingLiveSink {
        fn patch(&self, report_path: &Path, live: &crate::deck::LiveUpdate) -> Result<(), String> {
            let value = serde_json::to_value(live).map_err(|e| e.to_string())?;
            self.updates
                .borrow_mut()
                .push(value["step"].as_str().unwrap_or("").to_string());
            if self.patch_report {
                crate::deck::patch_live(report_path, live).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    struct PanicBdClient;

    impl crate::bd::BdClient for PanicBdClient {
        fn ready(&self, _repo: &Path) -> crate::bd::Result<Vec<crate::bd::Issue>> {
            panic!("bd write/read should not run")
        }
        fn show(&self, _repo: &Path, _id: &str) -> crate::bd::Result<crate::bd::Issue> {
            panic!("bd show should not run")
        }
        fn count(&self, _repo: &Path) -> crate::bd::Result<u64> {
            panic!("bd count should not run")
        }
        fn blocked(&self, _repo: &Path) -> crate::bd::Result<Vec<crate::bd::Issue>> {
            panic!("bd blocked should not run")
        }
        fn claim(
            &self,
            _repo: &Path,
            _id: &str,
            _actor: &str,
        ) -> crate::bd::Result<crate::bd::Issue> {
            panic!("bd claim should not run")
        }
        fn release(&self, _repo: &Path, _id: &str) -> crate::bd::Result<crate::bd::Issue> {
            panic!("bd release should not run")
        }
        fn close(
            &self,
            _repo: &Path,
            _id: &str,
            _reason: &str,
        ) -> crate::bd::Result<crate::bd::Issue> {
            panic!("bd close should not run")
        }
        fn comment(
            &self,
            _repo: &Path,
            _id: &str,
            _text: &str,
        ) -> crate::bd::Result<crate::bd::Comment> {
            panic!("bd comment should not run")
        }
        fn set_metadata(
            &self,
            _repo: &Path,
            _id: &str,
            _key: &str,
            _value: &str,
        ) -> crate::bd::Result<crate::bd::Issue> {
            panic!("bd set_metadata should not run")
        }
    }

    struct PanicExec;
    impl Exec for PanicExec {
        fn spawn(&self, _request: &SpawnRequest) -> crate::dispatch::Result<Box<dyn ChildProcess>> {
            panic!("exec should not run")
        }
    }

    struct PanicCommits;
    impl CommitProbe for PanicCommits {
        fn head(&self, _repo: &Path) -> crate::dispatch::Result<Option<String>> {
            panic!("commit probe should not run")
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BdEvent {
        Claim { id: String },
        Release { id: String },
        Close { id: String, reason: String },
        Comment { id: String, text: String },
    }

    struct RecordingBdClient {
        issue: RefCell<Issue>,
        events: RefCell<Vec<BdEvent>>,
    }

    impl RecordingBdClient {
        fn new(issue: Issue) -> Self {
            Self {
                issue: RefCell::new(issue),
                events: RefCell::new(Vec::new()),
            }
        }

        fn close_count(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|event| matches!(event, BdEvent::Close { .. }))
                .count()
        }

        fn release_count(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|event| matches!(event, BdEvent::Release { .. }))
                .count()
        }
    }

    impl BdClient for RecordingBdClient {
        fn ready(&self, _repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            Err(BdError::new("ready not implemented in fake"))
        }

        fn show(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Ok(self.issue.borrow().clone())
        }

        fn count(&self, _repo: &Path) -> crate::bd::Result<u64> {
            Err(BdError::new("count not implemented in fake"))
        }

        fn blocked(&self, _repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            Err(BdError::new("blocked not implemented in fake"))
        }

        fn claim(&self, _repo: &Path, id: &str, actor: &str) -> crate::bd::Result<Issue> {
            self.events
                .borrow_mut()
                .push(BdEvent::Claim { id: id.to_string() });
            let mut issue = self.issue.borrow_mut();
            issue.status = "in_progress".to_string();
            issue.assignee = Some(actor.to_string());
            Ok(issue.clone())
        }

        fn release(&self, _repo: &Path, id: &str) -> crate::bd::Result<Issue> {
            self.events
                .borrow_mut()
                .push(BdEvent::Release { id: id.to_string() });
            let mut issue = self.issue.borrow_mut();
            issue.status = "open".to_string();
            issue.assignee = None;
            Ok(issue.clone())
        }

        fn close(&self, _repo: &Path, id: &str, reason: &str) -> crate::bd::Result<Issue> {
            self.events.borrow_mut().push(BdEvent::Close {
                id: id.to_string(),
                reason: reason.to_string(),
            });
            let mut issue = self.issue.borrow_mut();
            issue.status = "closed".to_string();
            Ok(issue.clone())
        }

        fn comment(&self, _repo: &Path, id: &str, text: &str) -> crate::bd::Result<Comment> {
            self.events.borrow_mut().push(BdEvent::Comment {
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

    struct SandboxExec {
        spawns: RefCell<Vec<SpawnRequest>>,
    }

    impl SandboxExec {
        fn new() -> Self {
            Self {
                spawns: RefCell::new(Vec::new()),
            }
        }

        fn spawns(&self) -> Vec<SpawnRequest> {
            self.spawns.borrow().clone()
        }
    }

    impl Exec for SandboxExec {
        fn spawn(&self, request: &SpawnRequest) -> crate::dispatch::Result<Box<dyn ChildProcess>> {
            self.spawns.borrow_mut().push(request.clone());
            if request.argv.iter().any(|arg| arg == "senior-reviewer") {
                std::fs::write(&request.stdout_path, br#"{"verdict":"ship","findings":[]}"#)
                    .expect("write review stdout");
                std::fs::write(&request.stderr_path, b"").expect("write review stderr");
                return Ok(Box::new(FakeChild::immediate(ProcessStatus::code(0))));
            }
            if request.argv.first().map(String::as_str) == Some("pi") {
                std::fs::write(&request.stdout_path, b"worker ran\n").expect("write worker stdout");
                std::fs::write(&request.stderr_path, b"").expect("write worker stderr");
                std::fs::write(request.cwd.join("worker.txt"), b"done\n")
                    .expect("write worker file");
                run(&request.cwd, "git", &["add", "worker.txt"]);
                run(
                    &request.cwd,
                    "git",
                    &["commit", "-m", "worker: complete sandbox bead"],
                );
                return Ok(Box::new(FakeChild::delayed_success()));
            }
            if request.argv.first().map(String::as_str) == Some("sh") {
                let output = Command::new(&request.argv[0])
                    .args(&request.argv[1..])
                    .current_dir(&request.cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .expect("spawn verify shell");
                std::fs::write(&request.stdout_path, &output.stdout).expect("write verify stdout");
                std::fs::write(&request.stderr_path, &output.stderr).expect("write verify stderr");
                let code = output.status.code().unwrap_or(1);
                return Ok(Box::new(FakeChild::immediate(ProcessStatus::code(code))));
            }
            panic!("unexpected spawn argv: {:?}", request.argv)
        }
    }

    struct FallbackExec {
        spawns: RefCell<Vec<SpawnRequest>>,
    }

    impl FallbackExec {
        fn new() -> Self {
            Self {
                spawns: RefCell::new(Vec::new()),
            }
        }

        fn spawns(&self) -> Vec<SpawnRequest> {
            self.spawns.borrow().clone()
        }
    }

    impl Exec for FallbackExec {
        fn spawn(&self, request: &SpawnRequest) -> crate::dispatch::Result<Box<dyn ChildProcess>> {
            self.spawns.borrow_mut().push(request.clone());
            if request.argv.iter().any(|arg| arg == "primary-worker") {
                std::fs::write(&request.stdout_path, b"").expect("write primary stdout");
                std::fs::write(&request.stderr_path, b"429 quota exceeded\n")
                    .expect("write primary stderr");
                return Ok(Box::new(FakeChild::immediate(ProcessStatus::code(1))));
            }
            if request.argv.iter().any(|arg| arg == "fallback-worker") {
                std::fs::write(&request.stdout_path, b"fallback worker ran\n")
                    .expect("write fallback stdout");
                std::fs::write(&request.stderr_path, b"").expect("write fallback stderr");
                std::fs::write(request.cwd.join("worker.txt"), b"done\n")
                    .expect("write worker file");
                run(&request.cwd, "git", &["add", "worker.txt"]);
                run(
                    &request.cwd,
                    "git",
                    &["commit", "-m", "worker: fallback complete sandbox bead"],
                );
                return Ok(Box::new(FakeChild::delayed_success()));
            }
            if request.argv.first().map(String::as_str) == Some("sh") {
                let output = Command::new(&request.argv[0])
                    .args(&request.argv[1..])
                    .current_dir(&request.cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .expect("spawn verify shell");
                std::fs::write(&request.stdout_path, &output.stdout).expect("write verify stdout");
                std::fs::write(&request.stderr_path, &output.stderr).expect("write verify stderr");
                let code = output.status.code().unwrap_or(1);
                return Ok(Box::new(FakeChild::immediate(ProcessStatus::code(code))));
            }
            panic!("unexpected spawn argv: {:?}", request.argv)
        }
    }

    struct FakeChild {
        waits: Rc<RefCell<Vec<Option<ProcessStatus>>>>,
        wait_result: ProcessStatus,
    }

    impl FakeChild {
        fn delayed_success() -> Self {
            Self {
                waits: Rc::new(RefCell::new(vec![None, Some(ProcessStatus::code(0))])),
                wait_result: ProcessStatus::code(0),
            }
        }

        fn immediate(status: ProcessStatus) -> Self {
            Self {
                waits: Rc::new(RefCell::new(vec![Some(status)])),
                wait_result: status,
            }
        }
    }

    impl ChildProcess for FakeChild {
        fn wait_for(
            &mut self,
            _timeout: Duration,
        ) -> crate::dispatch::Result<Option<ProcessStatus>> {
            Ok(self.waits.borrow_mut().remove(0))
        }

        fn terminate(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }
        fn kill(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }
        fn wait(&mut self) -> crate::dispatch::Result<ProcessStatus> {
            Ok(self.wait_result)
        }
    }

    #[test]
    fn codex_workers_are_metered_and_use_the_codex_bursar_provider() {
        let cfg = config::parse_str(
            "\
[[roster]]
name = \"gpt-5.6-sol\"
tier = \"lead\"
ceiling = \"XL\"
efficiency = \"heavy\"
backend = \"codex\"
dispatch_id = \"gpt-5.6-sol\"
reasoning_effort = \"max\"
provider = \"openai-codex\"
",
        )
        .expect("Codex roster parses");

        assert!(is_metered_worker_backend(Backend::Codex));
        assert_eq!(bursar_provider_for(&cfg.roster[0]), "codex");
    }
}
