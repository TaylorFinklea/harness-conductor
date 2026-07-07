//! cycle orchestration: scan → triage → plan → publish
//!
//! `conductor cycle --dry-run` wires the existing scan/triage/deck modules into
//! a single end-to-end pass that produces a harness-deck report and a journal
//! entry without any bd writes (no claims, no dispatches, no mutations).

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::bd::BdClient;
use crate::config::{Config, CostPolicy};
use crate::deck::{self, Bar, Block, CalloutLevel, Metric, Report, ReportStatus};
use crate::plan::CyclePlan;
use crate::scan::{self, RepoSnapshot, SkipReason, ZeroState};
use crate::state::{self, JournalEntry, JournalSummary};
use crate::triage::{self, Flag, Plan, RatchetState, SkipCode};

/// Errors from the cycle pipeline.
#[derive(Debug)]
pub(crate) struct CycleError {
    message: String,
}

impl CycleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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

/// Runs a dry-run cycle: scan → triage → plan → publish.
///
/// No bd mutations: no claims, no dispatches, no metadata writes.
/// Generates a cycle-id from the current UTC time.
pub(crate) fn run_dry_run(
    cfg: &Config,
    client: &dyn BdClient,
    reports_home: &Path,
    state_dir: &Path,
) -> Result<CycleResult, CycleError> {
    let now = Utc::now();
    let cycle_id = now.format("cycle-%Y%m%d-%H%M%S").to_string();
    let created_at = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    run_dry_run_with_timestamps(cfg, client, reports_home, state_dir, &cycle_id, &created_at)
}

/// Runs a dry-run cycle with explicit timestamps (for deterministic tests).
pub(crate) fn run_dry_run_with_timestamps(
    cfg: &Config,
    client: &dyn BdClient,
    reports_home: &Path,
    state_dir: &Path,
    cycle_id: &str,
    created_at: &str,
) -> Result<CycleResult, CycleError> {
    // 1. Scan
    let snapshots =
        scan::scan(&cfg.scan, client).map_err(|e| CycleError::new(format!("scan: {e}")))?;

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
    let cycle_plan = CyclePlan::from_triage(cycle_id, created_at, &plan);
    cycle_plan
        .save(state_dir)
        .map_err(|e| CycleError::new(format!("plan save: {e}")))?;

    // 4. Build and write harness-deck report
    let report = build_report(cycle_id, created_at, &snapshots, &plan)?;
    let report_path = deck::write_report(reports_home, &report)
        .map_err(|e| CycleError::new(format!("report: {e}")))?;

    // 5. Write journal entry
    let summary = compute_summary(&snapshots, &plan);
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

fn build_report(
    cycle_id: &str,
    created_at: &str,
    snapshots: &[RepoSnapshot],
    plan: &Plan,
) -> Result<Report, CycleError> {
    let mut blocks = Vec::new();

    // --- Metrics block ---
    let repos_scanned = snapshots.len();
    let ready_items: usize = snapshots.iter().map(|s| s.ready.len()).sum();
    let triaged_count = plan.proposals.len() + plan.dispatches.len();
    let flagged_count = plan.flags.len();
    let triaged_pct = (triaged_count * 100).checked_div(ready_items).unwrap_or(0);

    blocks.push(Block::metrics(
        "Cycle Metrics",
        vec![
            Metric::new("Repos scanned", repos_scanned.to_string()),
            Metric::new("Ready items", ready_items.to_string()),
            Metric::new("Triaged", triaged_pct.to_string()).with_unit("%"),
            Metric::new("Proposed", plan.proposals.len().to_string()),
            Metric::new("Dispatched", plan.dispatches.len().to_string()),
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

    // --- Approval block (informational in dry-run) ---
    let dispatch_summary = format_dispatch_plan(plan);
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

fn format_dispatch_plan(plan: &Plan) -> String {
    if plan.dispatches.is_empty() && plan.proposals.is_empty() {
        return "No dispatchable items.".to_string();
    }
    let mut lines = Vec::new();
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
    lines.join("\n")
}

fn compute_summary(snapshots: &[RepoSnapshot], plan: &Plan) -> JournalSummary {
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
    use crate::config;
    use serde_json::json;
    use std::cell::RefCell;
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

    // --- The test ---

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
        assert!(scan_gap_callout["markdown"]
            .as_str()
            .unwrap()
            .contains("bad"));

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
