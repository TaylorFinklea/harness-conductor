//! cycle orchestration: scan → triage → plan → publish
//!
//! `conductor cycle --dry-run` wires the existing scan/triage/deck modules into
//! a single end-to-end pass that produces a harness-deck report and a journal
//! entry without any bd writes (no claims, no dispatches, no mutations).

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::bd::BdClient;
use crate::bursar::BursarClient;
use crate::config::{Config, CostPolicy};
use crate::deck::{self, Bar, Block, CalloutLevel, Metric, Report, ReportStatus};
use crate::fields::{Triage, extract};
use crate::plan::{
    ApprovalScope, ApprovalScopeKind, CyclePlan, ItemAuthorizationRecord, ProviderRouteRecord,
    ScopeSelector, item_authorization_hash,
};
use crate::scan::{self, RepoSnapshot, SkipReason, ZeroState};
use crate::state::{self, JournalEntry, JournalSummary};
use crate::triage::{self, Flag, Plan, RatchetState, SkipCode};

/// Errors from the cycle pipeline.
#[derive(Debug)]
pub(crate) struct CycleError {
    message: String,
    scope: bool,
}

impl CycleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            scope: false,
        }
    }

    fn scope(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            scope: true,
        }
    }

    pub(crate) const fn is_scope_error(&self) -> bool {
        self.scope
    }
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CycleError {}

/// Successful cycle outcome.
pub(crate) struct CycleResult {
    pub(crate) cycle_id: String,
    pub(crate) report_path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CycleScopeRequest {
    pub(crate) repos: Vec<String>,
    pub(crate) only: Vec<String>,
}

/// Runs a dry-run cycle: scan → triage → plan → publish.
///
/// No bd mutations: no claims, no dispatches, no metadata writes.
/// Generates a cycle-id from the current UTC time.
pub(crate) fn run_dry_run(
    cfg: &Config,
    client: &dyn BdClient,
    bursar: &dyn BursarClient,
    reports_home: &Path,
    state_dir: &Path,
) -> Result<CycleResult, CycleError> {
    run_dry_run_scoped(
        cfg,
        client,
        bursar,
        reports_home,
        state_dir,
        &CycleScopeRequest::default(),
    )
}

pub(crate) fn run_dry_run_scoped(
    cfg: &Config,
    client: &dyn BdClient,
    bursar: &dyn BursarClient,
    reports_home: &Path,
    state_dir: &Path,
    scope: &CycleScopeRequest,
) -> Result<CycleResult, CycleError> {
    let now = Utc::now();
    let cycle_id = now.format("cycle-%Y%m%d-%H%M%S").to_string();
    let cycle_id = unique_cycle_id(state_dir, &cycle_id);
    let created_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    run_dry_run_with_timestamps_scoped(
        cfg,
        client,
        bursar,
        reports_home,
        state_dir,
        &cycle_id,
        &created_at,
        scope,
    )
}

/// Ensures a unique cycle-id within `state_dir`, appending a monotonic `-N`
/// suffix when the second-granular `base` would collide with an existing
/// plan. Without this, two cycles started within the same second (manual
/// re-run, cron double-fire) share an id and clobber each other's plan,
/// report, and log state.
fn unique_cycle_id(state_dir: &Path, base: &str) -> String {
    let plans_dir = state_dir.join("plans");
    let plan_path = |id: &str| plans_dir.join(format!("{id}.json"));
    if !plan_path(base).exists() {
        return base.to_string();
    }
    let mut counter = 2_u64;
    loop {
        let candidate = format!("{base}-{counter}");
        if !plan_path(&candidate).exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// Runs a dry-run cycle with explicit timestamps (for deterministic tests).
pub(crate) fn run_dry_run_with_timestamps(
    cfg: &Config,
    client: &dyn BdClient,
    bursar: &dyn BursarClient,
    reports_home: &Path,
    state_dir: &Path,
    cycle_id: &str,
    created_at: &str,
) -> Result<CycleResult, CycleError> {
    run_dry_run_with_timestamps_scoped(
        cfg,
        client,
        bursar,
        reports_home,
        state_dir,
        cycle_id,
        created_at,
        &CycleScopeRequest::default(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "deterministic test seam keeps timestamp and approval scope explicit"
)]
pub(crate) fn run_dry_run_with_timestamps_scoped(
    cfg: &Config,
    client: &dyn BdClient,
    bursar: &dyn BursarClient,
    reports_home: &Path,
    state_dir: &Path,
    cycle_id: &str,
    created_at: &str,
    scope_request: &CycleScopeRequest,
) -> Result<CycleResult, CycleError> {
    // 1. Scan
    let snapshots =
        scan::scan(&cfg.scan, client).map_err(|e| CycleError::new(format!("scan: {e}")))?;
    let resolved_scope = apply_scope(snapshots, scope_request)?;
    let snapshots = resolved_scope.snapshots;

    // 2. Triage (dry-run: all ratchets locked → propose-only)
    let ratchet: HashMap<String, RatchetState> = HashMap::new();
    let repo_cost_policy_by_repo: HashMap<String, CostPolicy> = cfg
        .repo_policies
        .iter()
        .map(|p| (p.repo.clone(), p.cost_policy))
        .collect();
    let plan = triage::route(
        &snapshots,
        &cfg.roster,
        &cfg.budgets,
        &ratchet,
        &repo_cost_policy_by_repo,
    );

    // 3. Build and save cycle plan
    let provider_advice = provider_route_advice(cfg, &snapshots, &plan, bursar)?;
    let mut cycle_plan = CyclePlan::from_triage(cycle_id, created_at, &plan);
    cycle_plan.apply_provider_routes(provider_advice);
    let max_dispatch_count = match resolved_scope.kind {
        ApprovalScopeKind::FleetAudit => cycle_plan.dispatches.len(),
        ApprovalScopeKind::RepositoryScope | ApprovalScopeKind::ExactItemScope => {
            cycle_plan.dispatches.len() + cycle_plan.proposals.len()
        }
    };
    cycle_plan.approval_scope = ApprovalScope::new(
        resolved_scope.kind,
        resolved_scope.selectors,
        resolved_scope.repo_paths,
        max_dispatch_count,
    )
    .map_err(|error| CycleError::scope(format!("scope: {error}")))?;
    cycle_plan.item_authorizations = build_item_authorizations(&snapshots, &cycle_plan)?;
    cycle_plan
        .save(state_dir)
        .map_err(|e| CycleError::new(format!("plan save: {e}")))?;

    // 4. Build and write harness-deck report
    let report = build_report(cycle_id, created_at, &snapshots, &plan, &cycle_plan)?;
    let report_path = deck::write_report(reports_home, &report)
        .map_err(|e| CycleError::new(format!("report: {e}")))?;

    // 5. Write journal entry
    let summary = compute_summary(&snapshots, &cycle_plan);
    let entry = JournalEntry {
        id: cycle_id.to_string(),
        completed_at: created_at.to_string(),
        dry_run: true,
        summary,
    };
    state::write_journal(state_dir, &entry)
        .map_err(|e| CycleError::new(format!("journal: {e}")))?;

    Ok(CycleResult {
        cycle_id: cycle_id.to_string(),
        report_path,
    })
}

#[derive(Debug)]
struct ResolvedScope {
    snapshots: Vec<RepoSnapshot>,
    kind: ApprovalScopeKind,
    selectors: Vec<ScopeSelector>,
    repo_paths: Vec<String>,
}

#[expect(
    clippy::too_many_lines,
    reason = "scope resolution keeps cross-selector validation in one fail-closed boundary"
)]
fn apply_scope(
    snapshots: Vec<RepoSnapshot>,
    request: &CycleScopeRequest,
) -> Result<ResolvedScope, CycleError> {
    if request.repos.is_empty() && request.only.is_empty() {
        let repo_paths = snapshots
            .iter()
            .filter(|snapshot| snapshot.is_beads_repo && snapshot.skip_reason.is_none())
            .map(canonical_snapshot_path)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(ResolvedScope {
            snapshots,
            kind: ApprovalScopeKind::FleetAudit,
            selectors: Vec::new(),
            repo_paths,
        });
    }

    let mut selected_repo_paths = Vec::new();
    let mut selected_repo_set = HashSet::new();
    for raw in &request.repos {
        let snapshot = resolve_repo(&snapshots, raw)?;
        reject_scoped_skip(snapshot)?;
        let canonical = canonical_snapshot_path(snapshot)?;
        if !selected_repo_set.insert(canonical.clone()) {
            return Err(CycleError::scope(format!(
                "scope: duplicate repository selector {raw}"
            )));
        }
        selected_repo_paths.push(canonical);
    }

    if request.only.is_empty() {
        let selected: Vec<RepoSnapshot> = snapshots
            .into_iter()
            .filter(|snapshot| {
                canonical_snapshot_path(snapshot)
                    .is_ok_and(|path| selected_repo_set.contains(&path))
            })
            .collect();
        if selected.iter().all(|snapshot| snapshot.ready.is_empty()) {
            return Err(CycleError::scope(
                "scope: repository selectors produced no ready items",
            ));
        }
        let selectors = selected_repo_paths
            .iter()
            .map(|repo| ScopeSelector::Repository { repo: repo.clone() })
            .collect();
        return Ok(ResolvedScope {
            snapshots: selected,
            kind: ApprovalScopeKind::RepositoryScope,
            selectors,
            repo_paths: selected_repo_paths,
        });
    }

    let mut exact = Vec::new();
    let mut exact_set = HashSet::new();
    let mut exact_repo_paths = HashSet::new();
    for raw in &request.only {
        let (repo_raw, issue_id) = raw
            .rsplit_once(':')
            .ok_or_else(|| CycleError::scope(format!("scope: invalid --only selector {raw:?}")))?;
        if repo_raw.is_empty() || issue_id.is_empty() {
            return Err(CycleError::scope(format!(
                "scope: invalid --only selector {raw:?}"
            )));
        }
        let snapshot = resolve_repo(&snapshots, repo_raw)?;
        reject_scoped_skip(snapshot)?;
        let canonical = canonical_snapshot_path(snapshot)?;
        if !selected_repo_set.is_empty() && !selected_repo_set.contains(&canonical) {
            return Err(CycleError::scope(format!(
                "scope: --only selector {raw:?} is outside the --repo filter"
            )));
        }
        if !snapshot.ready.iter().any(|issue| issue.id == issue_id) {
            return Err(CycleError::scope(format!(
                "scope: unknown ready issue {repo_raw}:{issue_id}"
            )));
        }
        if !exact_set.insert((canonical.clone(), issue_id.to_string())) {
            return Err(CycleError::scope(format!(
                "scope: duplicate exact-item selector {raw}"
            )));
        }
        exact_repo_paths.insert(canonical.clone());
        exact.push(ScopeSelector::ExactItem {
            repo: canonical,
            issue_id: issue_id.to_string(),
        });
    }

    let mut selected = Vec::new();
    for mut snapshot in snapshots {
        let canonical = canonical_snapshot_path(&snapshot)?;
        snapshot
            .ready
            .retain(|issue| exact_set.contains(&(canonical.clone(), issue.id.clone())));
        if !snapshot.ready.is_empty() {
            selected.push(snapshot);
        }
    }
    if selected.is_empty() {
        return Err(CycleError::scope(
            "scope: exact-item selectors produced no ready items",
        ));
    }
    Ok(ResolvedScope {
        snapshots: selected,
        kind: ApprovalScopeKind::ExactItemScope,
        selectors: exact,
        repo_paths: exact_repo_paths.into_iter().collect(),
    })
}

fn resolve_repo<'a>(
    snapshots: &'a [RepoSnapshot],
    selector: &str,
) -> Result<&'a RepoSnapshot, CycleError> {
    if let Some(snapshot) = snapshots.iter().find(|snapshot| snapshot.name == selector) {
        return Ok(snapshot);
    }
    let expanded = scan::expand_tilde(selector)
        .map_err(|error| CycleError::scope(format!("scope: {error}")))?;
    let selector_path = std::fs::canonicalize(&expanded).ok();
    snapshots
        .iter()
        .find(|snapshot| {
            snapshot.path == expanded
                || selector_path.as_ref().is_some_and(|path| {
                    std::fs::canonicalize(&snapshot.path).ok().as_ref() == Some(path)
                })
        })
        .ok_or_else(|| CycleError::scope(format!("scope: unknown repository {selector}")))
}

fn reject_scoped_skip(snapshot: &RepoSnapshot) -> Result<(), CycleError> {
    if !snapshot.is_beads_repo {
        return Err(CycleError::scope(format!(
            "scope: repository {} is not a beads repository",
            snapshot.name
        )));
    }
    if let Some(reason) = snapshot.skip_reason.as_ref() {
        return Err(CycleError::scope(format!(
            "scope: repository {} is excluded or unavailable: {reason:?}",
            snapshot.name
        )));
    }
    Ok(())
}

fn canonical_snapshot_path(snapshot: &RepoSnapshot) -> Result<String, CycleError> {
    let path = std::fs::canonicalize(&snapshot.path).map_err(|error| {
        CycleError::scope(format!(
            "scope: cannot canonicalize {}: {error}",
            snapshot.path.display()
        ))
    })?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| CycleError::scope("scope: canonical repository path is not UTF-8"))
}

fn build_item_authorizations(
    snapshots: &[RepoSnapshot],
    plan: &CyclePlan,
) -> Result<Vec<ItemAuthorizationRecord>, CycleError> {
    let mut items = plan
        .dispatches
        .iter()
        .map(|entry| {
            (
                entry.repo.as_str(),
                entry.issue_id.as_str(),
                entry.model.as_str(),
            )
        })
        .collect::<Vec<_>>();
    if !matches!(plan.approval_scope.kind, ApprovalScopeKind::FleetAudit) {
        items.extend(plan.proposals.iter().map(|entry| {
            (
                entry.repo.as_str(),
                entry.issue_id.as_str(),
                entry.model.as_str(),
            )
        }));
    }
    let mut records = Vec::with_capacity(items.len());
    for (repo, issue_id, selected_model) in items {
        let snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.name == repo)
            .ok_or_else(|| CycleError::new(format!("authorization: missing repo {repo}")))?;
        let issue = snapshot
            .ready
            .iter()
            .find(|issue| issue.id == issue_id)
            .ok_or_else(|| {
                CycleError::new(format!("authorization: missing issue {repo}/{issue_id}"))
            })?;
        let Triage::Triaged(routing) = extract(issue) else {
            return Err(CycleError::new(format!(
                "authorization: routing fields disappeared for {repo}/{issue_id}"
            )));
        };
        let route = plan
            .provider_routes
            .iter()
            .find(|route| route.repo == repo && route.issue_id == issue_id)
            .ok_or_else(|| {
                CycleError::new(format!("authorization: missing route {repo}/{issue_id}"))
            })?;
        let canonical = canonical_snapshot_path(snapshot)?;
        let sha256 = item_authorization_hash(
            &canonical,
            issue,
            &routing,
            selected_model,
            &route.approved_models,
        )
        .map_err(|error| CycleError::new(format!("authorization: {error}")))?;
        records.push(ItemAuthorizationRecord {
            repo: repo.to_string(),
            issue_id: issue_id.to_string(),
            sha256,
        });
    }
    records
        .sort_by(|left, right| (&left.repo, &left.issue_id).cmp(&(&right.repo, &right.issue_id)));
    Ok(records)
}

fn provider_route_advice(
    cfg: &Config,
    snapshots: &[RepoSnapshot],
    plan: &Plan,
    bursar: &dyn BursarClient,
) -> Result<Vec<(String, crate::route::RouteAdvice)>, CycleError> {
    let decisions =
        crate::route::snapshot_provider_decisions(bursar, &cfg.roster, cfg.budgets.use_bursar);
    let mut dispatch_count_by_model = HashMap::new();
    let mut advice = Vec::with_capacity(plan.dispatches.len() + plan.proposals.len());

    for (repo, issue_id, dispatched) in plan
        .dispatches
        .iter()
        .map(|entry| (entry.repo.as_str(), entry.issue_id.as_str(), true))
        .chain(
            plan.proposals
                .iter()
                .map(|entry| (entry.repo.as_str(), entry.issue_id.as_str(), false)),
        )
    {
        let issue = snapshots
            .iter()
            .find(|snapshot| snapshot.name == repo)
            .and_then(|snapshot| snapshot.ready.iter().find(|issue| issue.id == issue_id))
            .ok_or_else(|| {
                CycleError::new(format!(
                    "provider route: missing scan item {repo}/{issue_id}"
                ))
            })?;
        let Triage::Triaged(routing) = extract(issue) else {
            return Err(CycleError::new(format!(
                "provider route: routing fields disappeared for {repo}/{issue_id}"
            )));
        };
        let route = crate::route::select(
            repo,
            &routing,
            cfg.cost_policy_for(repo),
            &cfg.roster,
            &decisions,
            &dispatch_count_by_model,
            None,
        );
        if dispatched {
            if let Some(selected) = route.selected.as_ref() {
                *dispatch_count_by_model
                    .entry(selected.model.clone())
                    .or_insert(0) += 1;
            }
        }
        advice.push((issue_id.to_string(), route));
    }
    Ok(advice)
}

fn build_report(
    cycle_id: &str,
    created_at: &str,
    snapshots: &[RepoSnapshot],
    plan: &Plan,
    cycle_plan: &CyclePlan,
) -> Result<Report, CycleError> {
    let mut blocks = Vec::new();

    // --- Metrics block ---
    let repos_scanned = snapshots.len();
    let ready_items: usize = snapshots.iter().map(|s| s.ready.len()).sum();
    let triaged_count = cycle_plan.proposals.len() + cycle_plan.dispatches.len();
    let flagged_count = plan.flags.len();
    let triaged_pct = (triaged_count * 100).checked_div(ready_items).unwrap_or(0);

    blocks.push(Block::metrics(
        "Cycle Metrics",
        vec![
            Metric::new("Repos scanned", repos_scanned.to_string()),
            Metric::new("Ready items", ready_items.to_string()),
            Metric::new("Triaged", triaged_pct.to_string()).with_unit("%"),
            Metric::new("Proposed", cycle_plan.proposals.len().to_string()),
            Metric::new("Dispatched", cycle_plan.dispatches.len().to_string()),
            Metric::new("Flagged", flagged_count.to_string()),
        ],
        vec![Bar::new(
            "triaged",
            u8::try_from(triaged_pct.min(100)).expect("triaged_pct bounded via min(100)"),
            "cyan",
        )],
    ));

    // --- Table block (per-repo queue) ---
    let columns = vec!["Repo", "Ready", "State"];
    let rows: Vec<Vec<String>> = snapshots
        .iter()
        .map(|s| {
            let ready = if s.is_beads_repo && s.skip_reason.is_none() {
                s.ready.len().to_string()
            } else {
                "-".to_string()
            };
            let state = repo_state_str(s);
            vec![s.name.clone(), ready, state]
        })
        .collect();
    blocks.push(Block::table("Fleet Queue", columns, rows));

    if !cycle_plan.provider_routes.is_empty() {
        blocks.push(provider_route_audit_table(&cycle_plan.provider_routes));
    }

    // --- Approval block (informational in dry-run) ---
    let dispatch_summary = format_dispatch_plan(cycle_plan);
    blocks.push(Block::approval("dispatch-plan", dispatch_summary));

    blocks.extend(build_callouts(plan));

    Report::new(
        cycle_id,
        format!("Conductor dry-run: {cycle_id}"),
        created_at,
        ReportStatus::AwaitingReview,
        blocks,
    )
    .map_err(|e| CycleError::new(format!("report: {e}")))
}

fn provider_route_audit_table(routes: &[ProviderRouteRecord]) -> Block {
    let columns = vec![
        "Item",
        "Outcome",
        "Model",
        "Provider",
        "Backend",
        "Dispatch ID",
        "Reasoning",
        "Availability",
        "Source",
        "Checked",
        "Data as of",
        "Expires",
        "Expiry basis",
        "Action",
        "Reason",
        "Exclusions",
    ];
    let rows = routes.iter().flat_map(|route| {
        route.candidates.iter().map(|candidate| {
            vec![
                format!("{}/{}", route.repo, route.issue_id),
                candidate.outcome.clone(),
                candidate.model.clone(),
                candidate.provider.clone(),
                candidate.backend.clone(),
                candidate.dispatch_id.clone(),
                candidate.reasoning_effort.clone().unwrap_or_default(),
                candidate.availability.clone().unwrap_or_default(),
                candidate.source.clone().unwrap_or_default(),
                candidate.checked_at.clone().unwrap_or_default(),
                candidate.data_as_of.clone().unwrap_or_default(),
                candidate.expires_at.clone().unwrap_or_default(),
                candidate.expiry_basis.clone().unwrap_or_default(),
                candidate.action.clone().unwrap_or_default(),
                candidate.reason.clone().unwrap_or_default(),
                candidate
                    .exclusion_reasons
                    .iter()
                    .map(|reason| format!("{}: {}", reason.code, reason.reason))
                    .collect::<Vec<_>>()
                    .join("; "),
            ]
        })
    });
    Block::table("Provider Candidate Audit", columns, rows)
}

fn build_callouts(plan: &Plan) -> Vec<Block> {
    let mut callouts = Vec::new();

    // --- Callout blocks for flags ---
    let scan_gaps: Vec<String> = plan
        .flags
        .iter()
        .filter_map(|f| match f {
            Flag::ScanGap { repo, detail } => Some(format!("- {repo}: {detail}")),
            _ => None,
        })
        .collect();
    if !scan_gaps.is_empty() {
        callouts.push(Block::callout(
            CalloutLevel::Warn,
            "SCAN-GAP",
            format!(
                "{} repos had bd ready --json parse gaps:\n{}",
                scan_gaps.len(),
                scan_gaps.join("\n")
            ),
        ));
    }

    let untriaged: Vec<String> = plan
        .flags
        .iter()
        .filter_map(|f| match f {
            Flag::Untriaged {
                repo,
                issue_id,
                missing,
            } => Some(format!(
                "- {repo}/{issue_id}: missing {}",
                missing
                    .iter()
                    .map(|m| match m {
                        crate::fields::MissingField::TierFloor => "tier_floor",
                        crate::fields::MissingField::Complexity => "complexity",
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            _ => None,
        })
        .collect();
    if !untriaged.is_empty() {
        callouts.push(Block::callout(
            CalloutLevel::Warn,
            "UNTRIAGED",
            format!(
                "{} items missing routing fields:\n{}",
                untriaged.len(),
                untriaged.join("\n")
            ),
        ));
    }

    let over_ceiling: Vec<String> = plan
        .flags
        .iter()
        .filter_map(|f| match f {
            Flag::OverCeiling {
                repo,
                issue_id,
                complexity,
            } => Some(format!("- {repo}/{issue_id}: complexity {complexity:?}")),
            _ => None,
        })
        .collect();
    if !over_ceiling.is_empty() {
        callouts.push(Block::callout(
            CalloutLevel::Warn,
            "OVER-CEILING",
            format!(
                "{} items exceed every qualifying model ceiling:\n{}",
                over_ceiling.len(),
                over_ceiling.join("\n")
            ),
        ));
    }

    let budget_skips: Vec<&crate::triage::Skip> = plan
        .skips
        .iter()
        .filter(|s| s.reason == SkipCode::Budget)
        .collect();
    if !budget_skips.is_empty() {
        callouts.push(Block::callout(
            CalloutLevel::Info,
            "BUDGET",
            format!(
                "{} items skipped due to budget limits:\n{}",
                budget_skips.len(),
                budget_skips
                    .iter()
                    .map(|s| format!("- {}/{}", s.repo, s.issue_id))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ));
    }

    callouts
}

fn repo_state_str(s: &RepoSnapshot) -> String {
    if let Some(reason) = &s.skip_reason {
        return match reason {
            SkipReason::InProgress => "in-progress".to_string(),
            SkipReason::Excluded => "excluded".to_string(),
            SkipReason::NotBeadsRepo => "not-beads".to_string(),
            SkipReason::NotGitRepo => "not-git".to_string(),
            SkipReason::ScanGap { .. } => "scan-gap".to_string(),
        };
    }
    match s.zero_state {
        ZeroState::Drained => "drained".to_string(),
        ZeroState::Blocked => "blocked".to_string(),
        ZeroState::NotApplicable => "ready".to_string(),
    }
}

fn format_dispatch_plan(plan: &CyclePlan) -> String {
    let selectors = if plan.approval_scope.selectors.is_empty() {
        "none".to_string()
    } else {
        plan.approval_scope
            .selectors
            .iter()
            .map(|selector| match selector {
                ScopeSelector::Repository { repo } => repo.clone(),
                ScopeSelector::ExactItem { repo, issue_id } => format!("{repo}:{issue_id}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![
        format!("**Approval scope:** {}", plan.approval_scope.kind.label()),
        format!("**Selectors:** {selectors}"),
        format!(
            "**Maximum dispatch count:** {}",
            plan.approval_scope.max_dispatch_count
        ),
    ];
    if plan.dispatches.is_empty() && plan.proposals.is_empty() {
        lines.push("No dispatchable items.".to_string());
    }
    if !plan.proposals.is_empty() {
        lines.push(format!("**Proposed ({}):**", plan.proposals.len()));
        for p in &plan.proposals {
            lines.push(format!("- {}/{} → {}", p.repo, p.issue_id, p.model));
        }
    }
    if !plan.dispatches.is_empty() {
        lines.push(format!("**Dispatched ({}):**", plan.dispatches.len()));
        for d in &plan.dispatches {
            lines.push(format!(
                "- {}/{} → {} (verify: {})",
                d.repo, d.issue_id, d.model, d.verify_cmd
            ));
        }
    }
    if !plan.provider_routes.is_empty() {
        lines.push("**Approved provider envelopes:**".to_string());
        for route in &plan.provider_routes {
            if route.terminal_defer {
                lines.push(format!(
                    "- {}/{} → terminal defer (no approved model)",
                    route.repo, route.issue_id
                ));
            } else {
                lines.push(format!(
                    "- {}/{} → selected {}; approved [{}]",
                    route.repo,
                    route.issue_id,
                    route.selected_model.as_deref().unwrap_or("none"),
                    route.approved_models.join(", ")
                ));
            }
        }
    }
    lines.join("\n")
}

fn compute_summary(snapshots: &[RepoSnapshot], plan: &CyclePlan) -> JournalSummary {
    let ready: u64 = snapshots.iter().map(|s| s.ready.len() as u64).sum();
    JournalSummary {
        scanned: snapshots.len() as u64,
        ready,
        dispatched: plan.dispatches.len() as u64,
        proposed: plan.proposals.len() as u64,
        verified: 0,
        flagged: plan.flags.len() as u64,
        skipped: plan.skips.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdError, Comment, Issue};
    use crate::bursar::test_support::FakeBursarClient;
    use crate::bursar::{Availability, BursarClient, ProviderStatus, StatusReport};
    use crate::config;
    use serde_json::{Map, Value, json};
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // --- Fake BdClient ---

    struct FakeBdClient {
        ready: RefCell<HashMap<PathBuf, Vec<Issue>>>,
        ready_errors: RefCell<HashMap<PathBuf, BdError>>,
        count: RefCell<HashMap<PathBuf, u64>>,
        blocked: RefCell<HashMap<PathBuf, Vec<Issue>>>,
    }

    impl FakeBdClient {
        fn new() -> Self {
            Self {
                ready: RefCell::new(HashMap::new()),
                ready_errors: RefCell::new(HashMap::new()),
                count: RefCell::new(HashMap::new()),
                blocked: RefCell::new(HashMap::new()),
            }
        }

        fn set_ready(&self, repo: &Path, issues: Vec<Issue>) {
            self.ready.borrow_mut().insert(repo.to_path_buf(), issues);
        }

        fn set_ready_error(&self, repo: &Path, error: BdError) {
            self.ready_errors
                .borrow_mut()
                .insert(repo.to_path_buf(), error);
        }

        fn set_count(&self, repo: &Path, count: u64) {
            self.count.borrow_mut().insert(repo.to_path_buf(), count);
        }

        fn set_blocked(&self, repo: &Path, issues: Vec<Issue>) {
            self.blocked.borrow_mut().insert(repo.to_path_buf(), issues);
        }
    }

    impl BdClient for FakeBdClient {
        fn ready(&self, repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            if let Some(error) = self.ready_errors.borrow().get(repo).cloned() {
                return Err(error);
            }
            self.ready
                .borrow()
                .get(repo)
                .cloned()
                .ok_or_else(|| BdError::new(format!("no ready data for {}", repo.display())))
        }

        fn show(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("show not implemented in fake"))
        }

        fn count(&self, repo: &Path) -> crate::bd::Result<u64> {
            self.count
                .borrow()
                .get(repo)
                .copied()
                .ok_or_else(|| BdError::new(format!("no count data for {}", repo.display())))
        }

        fn blocked(&self, repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            self.blocked
                .borrow()
                .get(repo)
                .cloned()
                .ok_or_else(|| BdError::new(format!("no blocked data for {}", repo.display())))
        }

        fn claim(&self, _repo: &Path, _id: &str, _actor: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("claim not implemented in fake"))
        }

        fn release(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("release not implemented in fake"))
        }

        fn close(&self, _repo: &Path, _id: &str, _reason: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("close not implemented in fake"))
        }

        fn comment(&self, _repo: &Path, _id: &str, _text: &str) -> crate::bd::Result<Comment> {
            Err(BdError::new("comment not implemented in fake"))
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

    // --- Temp dir helper ---

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-cycle-{label}-{nanos}"));
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

    // --- Repo/issue builders ---

    fn init_git_repo(path: &Path) {
        let git_dir = path.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").expect("write HEAD");
        let refs_dir = git_dir.join("refs").join("heads");
        std::fs::create_dir_all(&refs_dir).expect("mkdir refs/heads");
        let main_ref = refs_dir.join("main");
        std::fs::write(&main_ref, "abc123\n").expect("write main ref");
    }

    fn init_beads_repo(path: &Path) {
        init_git_repo(path);
        let beads_dir = path.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("mkdir .beads");
        let metadata = beads_dir.join("metadata.json");
        std::fs::write(&metadata, r#"{"backend":"dolt"}"#).expect("write metadata.json");
    }

    #[test]
    fn unique_cycle_id_disambiguates_same_second() {
        let state = TempDir::new("cycle-id-collision-state");
        let plans_dir = state.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let base = "cycle-20260716-120000";

        // First cycle in the second: clean id, no plan yet.
        assert_eq!(unique_cycle_id(state.path(), base), base);

        // Seed the first cycle's plan so a same-second re-run sees a collision.
        std::fs::write(plans_dir.join(format!("{base}.json")), "{}").unwrap();

        // Second cycle in the same second: bumped, distinct, no clobber.
        assert_eq!(unique_cycle_id(state.path(), base), format!("{base}-2"));

        // Seed the bumped plan too; a third same-second run must keep advancing.
        std::fs::write(plans_dir.join(format!("{base}-2.json")), "{}").unwrap();
        assert_eq!(unique_cycle_id(state.path(), base), format!("{base}-3"));
    }

    fn make_issue_with_metadata(id: &str, priority: u32, tier: &str, complexity: &str) -> Issue {
        let mut metadata = BTreeMap::new();
        metadata.insert("tier_floor".to_string(), json!(tier));
        metadata.insert("complexity".to_string(), json!(complexity));
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: String::new(),
            acceptance_criteria: String::new(),
            notes: String::new(),
            status: "open".to_string(),
            priority,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
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

    fn make_untriaged_issue(id: &str, priority: u32) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: String::new(),
            acceptance_criteria: String::new(),
            notes: "no routing fields here".to_string(),
            status: "open".to_string(),
            priority,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            created_by: "test".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: None,
            labels: None,
            estimated_minutes: None,
            metadata: None,
            parent: None,
            dependencies: None,
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

    fn ready_json_error(output: &str) -> BdError {
        let err = serde_json::from_str::<Vec<Issue>>(output)
            .expect_err("fixture must fail as bd ready issue JSON");
        BdError::json("bd ready", &err)
    }

    struct CountingBursar {
        report: StatusReport,
        calls: Cell<usize>,
    }

    impl BursarClient for CountingBursar {
        fn status(&self) -> crate::bursar::Result<StatusReport> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.report.clone())
        }
    }

    fn provider_status_report() -> StatusReport {
        let checked = Utc::now();
        let checked_at = checked.to_rfc3339();
        let data_as_of = (checked - chrono::Duration::seconds(1)).to_rfc3339();
        let expires_at = (checked + chrono::Duration::minutes(15)).to_rfc3339();
        let providers = [
            ("anthropic", Availability::Unknown),
            ("codex", Availability::Healthy),
            ("opencode-go", Availability::Exhausted),
            ("agy", Availability::Unknown),
        ]
        .into_iter()
        .map(|(provider, availability)| {
            let mut extra = Map::new();
            extra.insert(
                "observation_model".to_string(),
                Value::String(format!("{provider}-observed-model")),
            );
            extra.insert(
                "observation_expiry_basis".to_string(),
                Value::String("human-override".to_string()),
            );
            (
                provider.to_string(),
                ProviderStatus {
                    availability,
                    source: "cycle-test".to_string(),
                    checked_at: checked_at.clone(),
                    data_as_of: Some(data_as_of.clone()),
                    expires_at: Some(expires_at.clone()),
                    windows: Vec::new(),
                    reason: (availability != Availability::Healthy)
                        .then(|| "fixture limit".to_string()),
                    extra,
                },
            )
        })
        .collect();
        StatusReport {
            schema: "bursar/status@2".to_string(),
            checked_at,
            providers,
        }
    }

    // --- The test ---

    fn scope_snapshot(
        path: PathBuf,
        name: &str,
        ready: Vec<Issue>,
        skip_reason: Option<SkipReason>,
    ) -> RepoSnapshot {
        RepoSnapshot {
            path,
            name: name.to_string(),
            is_beads_repo: true,
            skip_reason,
            count: ready.len() as u64,
            ready,
            blocked: Vec::new(),
            zero_state: ZeroState::NotApplicable,
            freshness: crate::scan::Freshness::Fresh,
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one selector matrix keeps canonical aliases and all fail-closed cases together"
    )]
    fn scope_resolution_is_canonical_and_rejects_invalid_selectors() {
        let fleet = TempDir::new("scope-resolution");
        let alpha = fleet.path().join("alpha");
        let beta = fleet.path().join("beta");
        let empty = fleet.path().join("empty");
        let excluded = fleet.path().join("chezmoi-personal");
        for path in [&alpha, &beta, &empty, &excluded] {
            std::fs::create_dir_all(path).unwrap();
        }
        let snapshots = vec![
            scope_snapshot(
                alpha.clone(),
                "alpha",
                vec![
                    make_issue_with_metadata("a-1", 1, "senior", "M"),
                    make_issue_with_metadata("a-2", 2, "senior", "M"),
                ],
                None,
            ),
            scope_snapshot(
                beta.clone(),
                "beta",
                vec![make_issue_with_metadata("b-1", 1, "senior", "M")],
                None,
            ),
            scope_snapshot(empty, "empty", Vec::new(), None),
            scope_snapshot(
                excluded,
                "chezmoi-personal",
                Vec::new(),
                Some(SkipReason::Excluded),
            ),
        ];

        let resolved = apply_scope(
            snapshots.clone(),
            &CycleScopeRequest {
                repos: vec!["alpha".to_string()],
                only: vec!["alpha:a-2".to_string()],
            },
        )
        .expect("exact scope");
        assert_eq!(resolved.kind, ApprovalScopeKind::ExactItemScope);
        assert_eq!(resolved.snapshots.len(), 1);
        assert_eq!(resolved.snapshots[0].ready[0].id, "a-2");
        assert_eq!(resolved.selectors.len(), 1);
        assert_eq!(
            resolved.repo_paths,
            vec![std::fs::canonicalize(&alpha).unwrap().to_str().unwrap()]
        );

        let repo_scope = apply_scope(
            snapshots.clone(),
            &CycleScopeRequest {
                repos: vec!["beta".to_string(), "alpha".to_string()],
                only: Vec::new(),
            },
        )
        .expect("repo scope");
        assert_eq!(repo_scope.kind, ApprovalScopeKind::RepositoryScope);
        assert_eq!(repo_scope.selectors.len(), 2);

        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        let home_aliases = apply_scope(
            vec![scope_snapshot(
                home.clone(),
                "home-repo",
                vec![make_issue_with_metadata("home-1", 1, "senior", "M")],
                None,
            )],
            &CycleScopeRequest {
                repos: vec![
                    "home-repo".to_string(),
                    home.display().to_string(),
                    "~".to_string(),
                ],
                only: Vec::new(),
            },
        )
        .expect_err("name, absolute, and tilde aliases are duplicates");
        assert!(home_aliases.is_scope_error());

        let invalid = [
            CycleScopeRequest {
                repos: vec!["alpha".to_string(), alpha.display().to_string()],
                only: Vec::new(),
            },
            CycleScopeRequest {
                repos: vec!["alpha".to_string()],
                only: vec!["beta:b-1".to_string()],
            },
            CycleScopeRequest {
                repos: Vec::new(),
                only: vec!["alpha:missing".to_string()],
            },
            CycleScopeRequest {
                repos: Vec::new(),
                only: vec!["alpha:a-1".to_string(), "alpha:a-1".to_string()],
            },
            CycleScopeRequest {
                repos: vec!["chezmoi-personal".to_string()],
                only: Vec::new(),
            },
            CycleScopeRequest {
                repos: vec!["empty".to_string()],
                only: Vec::new(),
            },
            CycleScopeRequest {
                repos: vec!["unknown".to_string()],
                only: Vec::new(),
            },
        ];
        for request in invalid {
            let error = apply_scope(snapshots.clone(), &request).expect_err("scope rejected");
            assert!(error.is_scope_error());
        }
    }

    fn scoped_config(root: &Path) -> Config {
        config::parse_str(&format!(
            r#"[scan]
root = "{}"

[budgets]
use_bursar = false

[[roster]]
name = "scope-worker"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "opencode-go/scope-worker"
provider = "opencode-go"
fallback = []
"#,
            root.display()
        ))
        .expect("scope config")
    }

    #[test]
    fn scoped_cycle_persists_exact_items_hashes_and_report_boundary() {
        let fleet = TempDir::new("scoped-cycle-fleet");
        let reports = TempDir::new("scoped-cycle-reports");
        let state = TempDir::new("scoped-cycle-state");
        let alpha = fleet.path().join("alpha");
        let beta = fleet.path().join("beta");
        for repo in [&alpha, &beta] {
            std::fs::create_dir_all(repo).unwrap();
            init_beads_repo(repo);
        }
        let bd = FakeBdClient::new();
        bd.set_ready(
            &alpha,
            vec![
                make_issue_with_metadata("a-1", 1, "senior", "M"),
                make_issue_with_metadata("a-2", 2, "senior", "M"),
            ],
        );
        bd.set_count(&alpha, 2);
        bd.set_blocked(&alpha, Vec::new());
        bd.set_ready(
            &beta,
            vec![make_issue_with_metadata("b-1", 1, "senior", "M")],
        );
        bd.set_count(&beta, 1);
        bd.set_blocked(&beta, Vec::new());

        let result = run_dry_run_with_timestamps_scoped(
            &scoped_config(fleet.path()),
            &bd,
            &FakeBursarClient::unavailable(),
            reports.path(),
            state.path(),
            "cycle-20260713-scoped",
            "2026-07-13T12:00:00Z",
            &CycleScopeRequest {
                repos: vec!["alpha".to_string()],
                only: vec!["alpha:a-2".to_string()],
            },
        )
        .expect("scoped cycle");

        let plan = CyclePlan::load(state.path(), "cycle-20260713-scoped").unwrap();
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].issue_id, "a-2");
        assert_eq!(plan.approval_scope.kind, ApprovalScopeKind::ExactItemScope);
        assert_eq!(plan.approval_scope.max_dispatch_count, 1);
        assert_eq!(plan.item_authorizations.len(), 1);
        assert_eq!(plan.item_authorizations[0].issue_id, "a-2");
        assert_eq!(plan.item_authorizations[0].sha256.len(), 64);
        let report: Value =
            serde_json::from_slice(&std::fs::read(result.report_path).unwrap()).unwrap();
        let approval = report["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "approval")
            .unwrap();
        let prompt = approval["prompt"].as_str().unwrap();
        assert!(prompt.contains("exact-item-scope"));
        assert!(prompt.contains("Maximum dispatch count:** 1"));
        assert!(prompt.contains("a-2"));
        assert!(!prompt.contains("a-1"));
        assert!(!prompt.contains("b-1"));
    }

    #[test]
    fn scope_fleet_audit_with_103_proposals_authorizes_zero_proposals() {
        let fleet = TempDir::new("scope-103-fleet");
        let reports = TempDir::new("scope-103-reports");
        let state = TempDir::new("scope-103-state");
        let repo = fleet.path().join("alpha");
        std::fs::create_dir_all(&repo).unwrap();
        init_beads_repo(&repo);
        let issues = (0..103)
            .map(|index| make_issue_with_metadata(&format!("a-{index:03}"), index, "senior", "M"))
            .collect::<Vec<_>>();
        let bd = FakeBdClient::new();
        bd.set_ready(&repo, issues);
        bd.set_count(&repo, 103);
        bd.set_blocked(&repo, Vec::new());

        run_dry_run_with_timestamps(
            &scoped_config(fleet.path()),
            &bd,
            &FakeBursarClient::unavailable(),
            reports.path(),
            state.path(),
            "cycle-20260713-fleet-103",
            "2026-07-13T12:00:00Z",
        )
        .expect("fleet audit");
        let plan = CyclePlan::load(state.path(), "cycle-20260713-fleet-103").unwrap();
        assert_eq!(plan.proposals.len(), 103);
        assert!(plan.dispatches.is_empty());
        assert_eq!(plan.approval_scope.kind, ApprovalScopeKind::FleetAudit);
        assert_eq!(plan.approval_scope.max_dispatch_count, 0);
        assert!(plan.item_authorizations.is_empty());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "single end-to-end assertion keeps saved-plan and report evidence aligned"
    )]
    fn cycle_persists_one_provider_snapshot_and_terminal_defers() {
        let fleet = TempDir::new("provider-fleet");
        let reports = TempDir::new("provider-reports");
        let state = TempDir::new("provider-state");
        let repo = fleet.path().join("alpha");
        std::fs::create_dir_all(&repo).unwrap();
        init_beads_repo(&repo);

        let config_src = format!(
            r#"[scan]
root = "{}"

[budgets]
use_bursar = true

[[roster]]
name = "healthy-junior"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "codex"
dispatch_id = "gpt-healthy"
reasoning_effort = "medium"
provider = "codex"
fallback = []

[[roster]]
name = "exhausted-senior"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "opencode-go/exhausted"
provider = "opencode-go"
fallback = []
"#,
            fleet.path().display()
        );
        let cfg = config::parse_str(&config_src).unwrap();
        let bd = FakeBdClient::new();
        bd.set_ready(
            &repo,
            vec![
                make_issue_with_metadata("healthy", 1, "junior", "S"),
                make_issue_with_metadata("deferred", 2, "senior", "M"),
            ],
        );
        bd.set_count(&repo, 2);
        bd.set_blocked(&repo, vec![]);
        let bursar = CountingBursar {
            report: provider_status_report(),
            calls: Cell::new(0),
        };

        let result = run_dry_run_with_timestamps(
            &cfg,
            &bd,
            &bursar,
            reports.path(),
            state.path(),
            "cycle-20260713-120000",
            "2026-07-13T12:00:00Z",
        )
        .unwrap();

        assert_eq!(bursar.calls.get(), 1, "one Bursar status call per cycle");
        let saved: Value = serde_json::from_slice(
            &std::fs::read(state.path().join("plans/cycle-20260713-120000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["proposals"].as_array().unwrap().len(), 1);
        assert_eq!(saved["proposals"][0]["model"], "healthy-junior");
        assert_eq!(saved["provider_routes"].as_array().unwrap().len(), 2);
        let healthy = &saved["provider_routes"][0];
        assert_eq!(healthy["selected_model"], "healthy-junior");
        assert_eq!(healthy["approved_models"][0], "healthy-junior");
        let healthy_candidate = &healthy["candidates"][0];
        assert_eq!(healthy_candidate["provider"], "codex");
        assert_eq!(healthy_candidate["backend"], "codex");
        assert_eq!(healthy_candidate["dispatch_id"], "gpt-healthy");
        assert_eq!(healthy_candidate["reasoning_effort"], "medium");
        assert_eq!(healthy_candidate["availability"], "healthy");
        assert_eq!(healthy_candidate["source"], "cycle-test");
        assert!(healthy_candidate["checked_at"].is_string());
        assert!(healthy_candidate["data_as_of"].is_string());
        assert!(healthy_candidate["expires_at"].is_string());
        assert_eq!(healthy_candidate["expiry_basis"], "human-override");
        assert_eq!(healthy_candidate["action"], "proceed");
        assert!(
            healthy_candidate["reason"]
                .as_str()
                .unwrap()
                .contains("bursar availability healthy")
        );
        assert_eq!(healthy_candidate["outcome"], "selected");

        let deferred = &saved["provider_routes"][1];
        assert_eq!(deferred["issue_id"], "deferred");
        assert_eq!(deferred["selected_model"], Value::Null);
        assert_eq!(deferred["approved_models"].as_array().unwrap().len(), 0);
        assert_eq!(deferred["terminal_defer"], true);
        assert!(
            deferred["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| {
                    candidate["provider"] == "opencode-go"
                        && candidate["availability"] == "exhausted"
                        && candidate["outcome"] == "excluded"
                        && candidate["exclusion_reasons"][0]["code"] == "provider-exhausted"
                })
        );

        let report: Value =
            serde_json::from_slice(&std::fs::read(result.report_path).unwrap()).unwrap();
        let audit = report["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "table" && block["title"] == "Provider Candidate Audit")
            .expect("provider candidate audit table");
        assert_eq!(audit["columns"].as_array().unwrap().len(), 16);
        assert!(audit["rows"].as_array().unwrap().iter().any(|row| {
            row[0] == "alpha/deferred"
                && row[2] == "exhausted-senior"
                && row[3] == "opencode-go"
                && row[7] == "exhausted"
                && row[15].as_str().unwrap().contains("provider-exhausted")
        }));
        let approval = report["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "approval")
            .unwrap();
        assert!(
            approval["prompt"]
                .as_str()
                .unwrap()
                .contains("alpha/deferred → terminal defer")
        );
    }

    fn assert_dry_run_report(result: &CycleResult, cycle_id: &str) {
        // --- Verify cycle-id ---
        assert_eq!(result.cycle_id, cycle_id);
        assert!(result.cycle_id.starts_with("cycle-"));
        assert_eq!(result.cycle_id.len(), 21);

        // --- Verify report file ---
        assert!(result.report_path.is_file());
        let report_bytes = std::fs::read(&result.report_path).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();

        assert_eq!(report["schema"], "harness-deck/report@1");
        assert_eq!(report["project"], "conductor");
        assert_eq!(report["harness"], "conductor");
        assert_eq!(report["id"], cycle_id);
        assert_eq!(report["status"], "awaiting-review");

        // --- Verify report blocks ---
        let blocks = report["blocks"].as_array().unwrap();
        let types: Vec<&str> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();

        assert!(types.contains(&"metrics"), "missing metrics block");
        assert!(types.contains(&"table"), "missing table block");
        assert!(types.contains(&"approval"), "missing approval block");
        assert!(types.contains(&"callout"), "missing callout block");

        // Verify approval block has id "dispatch-plan"
        let approval = blocks.iter().find(|b| b["type"] == "approval").unwrap();
        assert_eq!(approval["id"], "dispatch-plan");

        // Verify metrics values
        let metrics = blocks.iter().find(|b| b["type"] == "metrics").unwrap();
        let metric_items = metrics["metrics"].as_array().unwrap();
        let scanned = metric_items
            .iter()
            .find(|m| m["label"] == "Repos scanned")
            .unwrap();
        assert_eq!(scanned["value"], "3");

        let ready = metric_items
            .iter()
            .find(|m| m["label"] == "Ready items")
            .unwrap();
        assert_eq!(ready["value"], "4"); // 3 from alpha + 1 from beta

        // Verify table has all repos
        let table = blocks.iter().find(|b| b["type"] == "table").unwrap();
        let table_rows = table["rows"].as_array().unwrap();
        assert_eq!(table_rows.len(), 3); // alpha, beta, gamma
    }

    #[test]
    fn cycle_report_surfaces_scan_gaps() {
        let fleet = TempDir::new("scan-gap-fleet");
        let reports = TempDir::new("scan-gap-reports");
        let state = TempDir::new("scan-gap-state");

        let bad = fleet.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        init_beads_repo(&bad);

        let healthy = fleet.path().join("healthy");
        std::fs::create_dir_all(&healthy).unwrap();
        init_beads_repo(&healthy);

        let config_src = format!(
            r#"[scan]
root = "{}"

[budgets]
use_bursar = false

[[roster]]
name = "test-senior"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "test/senior"
"#,
            fleet.path().display()
        );
        let cfg = config::parse_str(&config_src).unwrap();

        let client = FakeBdClient::new();
        client.set_ready_error(&bad, ready_json_error("{"));
        client.set_ready(&healthy, vec![]);
        client.set_count(&healthy, 0);
        client.set_blocked(&healthy, vec![]);

        let cycle_id = "cycle-20260702-121500";
        let created_at = "2026-07-02T12:15:00Z";
        let result = run_dry_run_with_timestamps(
            &cfg,
            &client,
            &FakeBursarClient::unavailable(),
            reports.path(),
            state.path(),
            cycle_id,
            created_at,
        )
        .unwrap();

        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&result.report_path).unwrap()).unwrap();
        let blocks = report["blocks"].as_array().unwrap();

        let metrics = blocks.iter().find(|b| b["type"] == "metrics").unwrap();
        let flagged = metrics["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["label"] == "Flagged")
            .unwrap();
        assert_eq!(flagged["value"], "1");

        let table = blocks.iter().find(|b| b["type"] == "table").unwrap();
        let rows = table["rows"].as_array().unwrap();
        let bad_row = rows
            .iter()
            .find(|row| row[0] == "bad")
            .expect("bad repo row");
        assert_eq!(bad_row[1], "-");
        assert_eq!(bad_row[2], "scan-gap");

        let scan_gap_callout = blocks
            .iter()
            .find(|b| b["type"] == "callout" && b["tag"] == "SCAN-GAP")
            .expect("scan gap callout");
        assert!(
            scan_gap_callout["markdown"]
                .as_str()
                .unwrap()
                .contains("bad")
        );

        let plan_path = state.path().join("plans").join(format!("{cycle_id}.json"));
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(plan_path).unwrap()).unwrap();
        assert_eq!(plan["flags"][0]["kind"], "scan-gap");
        assert_eq!(plan["flags"][0]["repo"], "bad");
    }

    #[test]
    fn cycle_dry_run() {
        let fleet = TempDir::new("fleet");
        let reports = TempDir::new("reports");
        let state = TempDir::new("state");

        // Create fixture repos
        let repo_alpha = fleet.path().join("alpha");
        std::fs::create_dir_all(&repo_alpha).unwrap();
        init_beads_repo(&repo_alpha);

        let repo_beta = fleet.path().join("beta");
        std::fs::create_dir_all(&repo_beta).unwrap();
        init_beads_repo(&repo_beta);

        let repo_gamma = fleet.path().join("gamma");
        std::fs::create_dir_all(&repo_gamma).unwrap();
        init_beads_repo(&repo_gamma);

        // Config with a small roster so we can produce over-ceiling flags
        let config_src = format!(
            r#"[scan]
root = "{}"

[budgets]
max_dispatches_per_cycle = 8
max_active_per_repo = 1
max_external_dispatches = 4
use_bursar = false

[[roster]]
name = "test-senior"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "test/senior"

[[roster]]
name = "test-junior"
tier = "junior"
ceiling = "S"
efficiency = "lean"
backend = "agy"
dispatch_id = "test/junior"
"#,
            fleet.path().display()
        );

        let cfg = config::parse_str(&config_src).unwrap();

        // Set up FakeBdClient
        let client = FakeBdClient::new();

        // alpha: 3 ready issues (senior/M, junior/S, untriaged)
        client.set_ready(
            &repo_alpha,
            vec![
                make_issue_with_metadata("a1", 1, "senior", "M"),
                make_issue_with_metadata("a2", 2, "junior", "S"),
                make_untriaged_issue("a3", 3),
            ],
        );
        client.set_count(&repo_alpha, 3);
        client.set_blocked(&repo_alpha, vec![]);

        // beta: 1 ready issue (senior/XL → over-ceiling with this roster)
        client.set_ready(
            &repo_beta,
            vec![make_issue_with_metadata("b1", 1, "senior", "XL")],
        );
        client.set_count(&repo_beta, 1);
        client.set_blocked(&repo_beta, vec![]);

        // gamma: drained (0 ready, 0 count)
        client.set_ready(&repo_gamma, vec![]);
        client.set_count(&repo_gamma, 0);
        client.set_blocked(&repo_gamma, vec![]);

        // Run dry-run with deterministic timestamps
        let cycle_id = "cycle-20260702-120000";
        let created_at = "2026-07-02T12:00:00Z";
        let result = run_dry_run_with_timestamps(
            &cfg,
            &client,
            &FakeBursarClient::unavailable(),
            reports.path(),
            state.path(),
            cycle_id,
            created_at,
        )
        .unwrap();

        assert_dry_run_report(&result, cycle_id);

        // --- Verify journal ---
        let journal_path = state.path().join("journal.json");
        assert!(journal_path.is_file());
        let journal_bytes = std::fs::read(&journal_path).unwrap();
        let journal: serde_json::Value = serde_json::from_slice(&journal_bytes).unwrap();
        assert_eq!(journal["last_cycle"]["id"], cycle_id);
        assert_eq!(journal["last_cycle"]["dry_run"], true);
        assert_eq!(journal["last_cycle"]["completed_at"], created_at);
        assert_eq!(journal["last_cycle"]["summary"]["scanned"], 3);
        assert_eq!(journal["last_cycle"]["summary"]["ready"], 4);
        assert_eq!(journal["last_cycle"]["summary"]["dispatched"], 0);
        assert_eq!(journal["last_cycle"]["summary"]["verified"], 0);

        // --- Verify plan file ---
        let plan_path = state.path().join("plans").join(format!("{cycle_id}.json"));
        assert!(plan_path.is_file());
        let plan_bytes = std::fs::read(&plan_path).unwrap();
        let plan_json: serde_json::Value = serde_json::from_slice(&plan_bytes).unwrap();
        assert_eq!(plan_json["cycle_id"], cycle_id);

        // --- Verify no bd writes happened (dry-run invariant) ---
        // The FakeBdClient's claim/release/close/set_metadata all return errors,
        // so if the cycle tried any bd write, it would have failed.
        // The fact that we got here proves no bd writes were attempted.
    }
}
