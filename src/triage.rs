//! routing algorithm + gates + budgets (pure — the invariant test suite lives here)
//!
//! Pinned rules (conductor-v1-spec.md, "Routing algorithm" + "Invariants" — encoded
//! exactly, not reinterpreted).
//!
//! Orders: complexity `S<M<L<XL`; tier `junior<senior<lead`; efficiency `lean<std<heavy`.
//!
//! ### Routing algorithm
//! 1. Drop repos: excluded, any `in_progress` bead present, or repo already used
//!    this cycle (enforced here via a per-repo dispatch counter checked against
//!    `budgets.max_active_per_repo`).
//! 2. For each ready item with complete fields: candidate models = roster where
//!    `tier ≥ tier_floor` and `ceiling ≥ complexity`, grouped by tier; take the
//!    lowest qualifying tier, then most efficient; tie → fewer dispatches so far
//!    this cycle; then roster order.
//! 3. No candidate → flag `over-ceiling`.
//! 4. Apply budgets in priority order (bd priority asc, then oldest `created_at`):
//!    stop at any ceiling; excess → `skipped(budget)`.
//! 5. Lead-floor items are ALWAYS propose-only (never auto-dispatched by ratchet).
//!
//! ### The nine invariants this module's test suite encodes
//! 1. Closed roster — only roster models ever appear in a dispatch/proposal.
//! 2. `tier_floor` is a hard gate — unknown/unparseable floor flags, never guesses.
//! 3. Fail closed — no runnable `verify_cmd` ⇒ not dispatchable (falls back to proposal).
//! 4. One writer per repo — a pre-existing `in_progress` bead skips the whole
//!    repo; `budgets.max_active_per_repo` caps auto-dispatches per repo per cycle.
//! 5. Never dispatch an excluded repo (personal chezmoi hard-coded deny, in depth).
//! 6. Close only verified — every dispatch carries the `verify_cmd` precondition
//!    that downstream verification requires before any `bd close`.
//! 7. Budgets are ceilings, not targets — excess is `skipped(budget)`.
//! 8. No silent drops — every ready item lands in exactly one output bucket.
//! 9. Ratchet failure re-locks — a locked repo always proposes, never auto-dispatches.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::bd::Issue;
#[allow(unused_imports)]
use crate::config::{Backend, Budgets, Ceiling, Cost, CostPolicy, Efficiency, RosterEntry, Tier};
use crate::fields::{MissingField, RoutingFields, Triage, extract};
use crate::scan::{RepoSnapshot, SkipReason};

// ---------------------------------------------------------------------------
// Ratchet input (ratchet.rs owns the persisted counters and their mutation;
// this module only reads the current unlock/lock decision as a pure input).
// ---------------------------------------------------------------------------

/// Per-repo ratchet unlock state. Repos absent from the input map are treated
/// as `Locked` (fail closed — a repo must *earn* unlock; see invariant 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RatchetState {
    Locked,
    Unlocked,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// An item auto-executed this cycle without waiting for human approval.
/// Only reachable when `tier_floor` is senior/junior, a runnable `verify_cmd`
/// exists, the repo's ratchet is unlocked, and every budget still has room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dispatch {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
    pub(crate) verify_cmd: String,
}

/// An item with a valid candidate model that requires human approval before
/// it can run (lead-floor items, locked-ratchet repos, or missing `verify_cmd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proposal {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
}

/// Why an item couldn't be routed to any candidate model, or a fleet-level
/// escalation the user must resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flag {
    /// Missing/unparseable `tier_floor` and/or `complexity` (invariant 2).
    Untriaged {
        repo: String,
        issue_id: String,
        missing: Vec<MissingField>,
    },
    /// `complexity` exceeds every roster ceiling for a qualifying tier.
    OverCeiling {
        repo: String,
        issue_id: String,
        complexity: Ceiling,
    },
    /// Repo-level scan coverage gap; `bd ready --json` could not be parsed.
    ScanGap { repo: String, detail: String },
    /// Reserved for `conductor roster drift` (scorecard-vs-`conductor.toml`
    /// comparison) — a separate check outside this module's pure algorithm;
    /// never constructed here.
    RosterDrift,
}

/// Why an item that would otherwise route was not dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipCode {
    Excluded,
    InProgress,
    NotBeadsRepo,
    NotGitRepo,
    Budget,
}

/// An item this cycle saw but did not act on, with an accounted reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skip {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) reason: SkipCode,
}

/// The full triage output for one cycle. Invariant 8 (no silent drops):
/// every ready item the cycle saw appears in exactly one of these buckets.
#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    pub(crate) dispatches: Vec<Dispatch>,
    pub(crate) proposals: Vec<Proposal>,
    pub(crate) flags: Vec<Flag>,
    pub(crate) skips: Vec<Skip>,
}

// ---------------------------------------------------------------------------
// Order helpers (complexity S<M<L<XL; tier junior<senior<lead; efficiency lean<std<heavy)
// ---------------------------------------------------------------------------

const fn tier_rank(t: Tier) -> u8 {
    match t {
        Tier::Junior => 0,
        Tier::Senior => 1,
        Tier::Lead => 2,
    }
}

const fn ceiling_rank(c: Ceiling) -> u8 {
    match c {
        Ceiling::S => 0,
        Ceiling::M => 1,
        Ceiling::L => 2,
        Ceiling::Xl => 3,
    }
}

const fn efficiency_rank(e: Efficiency) -> u8 {
    match e {
        Efficiency::Lean => 0,
        Efficiency::Std => 1,
        Efficiency::Heavy => 2,
    }
}

fn skip_code_for(reason: &SkipReason) -> Option<SkipCode> {
    match reason {
        SkipReason::Excluded => Some(SkipCode::Excluded),
        SkipReason::InProgress => Some(SkipCode::InProgress),
        SkipReason::NotBeadsRepo => Some(SkipCode::NotBeadsRepo),
        SkipReason::NotGitRepo => Some(SkipCode::NotGitRepo),
        SkipReason::ScanGap { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Candidate selection (Routing algorithm, step 2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateRejection {
    BelowTierFloor { required: Tier, actual: Tier },
    BelowCeiling { required: Ceiling, actual: Ceiling },
    CostPolicy { policy: CostPolicy, cost: Cost },
}

pub(crate) fn candidate_rejection(
    roster: &RosterEntry,
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
) -> Option<CandidateRejection> {
    if tier_rank(roster.tier) < tier_rank(routing.tier_floor) {
        return Some(CandidateRejection::BelowTierFloor {
            required: routing.tier_floor,
            actual: roster.tier,
        });
    }
    if ceiling_rank(roster.ceiling) < ceiling_rank(routing.complexity) {
        return Some(CandidateRejection::BelowCeiling {
            required: routing.complexity,
            actual: roster.ceiling,
        });
    }
    if !routing.trains_ok && !repo_cost_policy.allows(roster.cost) {
        return Some(CandidateRejection::CostPolicy {
            policy: repo_cost_policy,
            cost: roster.cost,
        });
    }
    None
}

/// Selects the routing target for one triaged item: roster entries with
/// `tier ≥ tier_floor` and `ceiling ≥ complexity`, narrowed to the lowest
/// qualifying tier, then the most efficient, then fewest dispatches so far
/// this cycle, then roster order. `None` means over-ceiling (no candidate).
///
/// Cost-axis gate: a model whose `cost` is `FreeTrainsInput` is only eligible
/// when the repo's `CostPolicy` allows it (`oss`/`public`), OR the item opts
/// in via `routing.trains_ok`. Applied after the tier/ceiling filter, before
/// the efficiency tiebreak, so a free-train model never shadows a paid one on
/// a proprietary repo.
fn select_candidate<'a>(
    roster: &'a [RosterEntry],
    routing: &RoutingFields,
    repo: &str,
    repo_cost_policy: CostPolicy,
    dispatch_count_by_model: &HashMap<String, u32>,
) -> Option<&'a RosterEntry> {
    crate::route::select_legacy(
        roster,
        routing,
        repo,
        repo_cost_policy,
        dispatch_count_by_model,
    )
}

// ---------------------------------------------------------------------------
// Routing entry point
// ---------------------------------------------------------------------------

/// Runs the pure triage core over one cycle's fleet snapshot, producing a
/// `Plan`. No IO: all inputs are already-gathered data.
///
/// `repo_cost_policy_by_repo` is a per-repo lookup of `CostPolicy`; repos
/// absent from the map default to `CostPolicy::Proprietary` (fail closed for
/// `FreeTrainsInput` models). In production this is built from the config's
/// `[[repo_policy]]` entries (see `Config::cost_policy_for`).
#[expect(
    clippy::too_many_lines,
    reason = "routing invariant flow is kept linear for auditability"
)]
pub(crate) fn route(
    repos: &[RepoSnapshot],
    roster: &[RosterEntry],
    budgets: &Budgets,
    ratchet: &HashMap<String, RatchetState>,
    repo_cost_policy_by_repo: &HashMap<String, CostPolicy>,
) -> Plan {
    let mut plan = Plan::default();

    // Step 1: drop excluded / in_progress / not-a-beads-repo / not-a-git-repo
    // repos entirely — every ready item they carry is accounted as skipped.
    // Scan gaps have no trusted ready list, so they become repo-level flags.
    // Everything else joins the priority queue for per-item routing.
    let mut queue: Vec<(&str, &Issue)> = Vec::new();
    for repo in repos {
        if let Some(reason) = &repo.skip_reason {
            if let SkipReason::ScanGap { message, .. } = reason {
                plan.flags.push(Flag::ScanGap {
                    repo: repo.name.clone(),
                    detail: message.clone(),
                });
                continue;
            }

            if let Some(code) = skip_code_for(reason) {
                for issue in &repo.ready {
                    plan.skips.push(Skip {
                        repo: repo.name.clone(),
                        issue_id: issue.id.clone(),
                        reason: code,
                    });
                }
            }
            continue;
        }
        for issue in &repo.ready {
            queue.push((repo.name.as_str(), issue));
        }
    }

    // Ordering: bd priority asc, then created_at asc (oldest first).
    queue.sort_by(|a, b| {
        a.1.priority
            .cmp(&b.1.priority)
            .then_with(|| a.1.created_at.cmp(&b.1.created_at))
    });

    let mut dispatch_count_by_model: HashMap<String, u32> = HashMap::new();
    let mut dispatch_count_by_repo: HashMap<&str, u32> = HashMap::new();
    let mut global_dispatch_count: u32 = 0;
    let mut global_external_count: u32 = 0;

    for (repo_name, issue) in queue {
        match extract(issue) {
            Triage::Untriaged { missing } => {
                // Invariant 2: unknown/unparseable tier_floor/complexity flags,
                // never guesses.
                plan.flags.push(Flag::Untriaged {
                    repo: repo_name.to_string(),
                    issue_id: issue.id.clone(),
                    missing,
                });
            }
            Triage::Triaged(routing) => {
                let repo_policy = repo_cost_policy_by_repo
                    .get(repo_name)
                    .copied()
                    .unwrap_or_default();
                let Some(chosen) = select_candidate(
                    roster,
                    &routing,
                    repo_name,
                    repo_policy,
                    &dispatch_count_by_model,
                ) else {
                    // Step 3: no candidate qualifies (complexity above every
                    // qualifying ceiling) -> over-ceiling flag.
                    plan.flags.push(Flag::OverCeiling {
                        repo: repo_name.to_string(),
                        issue_id: issue.id.clone(),
                        complexity: routing.complexity,
                    });
                    continue;
                };

                // Step 5 + invariants 3/9: lead-floor items always propose;
                // a missing verify_cmd or a locked ratchet also forces a
                // proposal instead of an auto-dispatch.
                let unlocked = matches!(ratchet.get(repo_name), Some(RatchetState::Unlocked));
                let auto_dispatch_eligible =
                    routing.tier_floor != Tier::Lead && routing.verify_cmd.is_some() && unlocked;

                if !auto_dispatch_eligible {
                    plan.proposals.push(Proposal {
                        repo: repo_name.to_string(),
                        issue_id: issue.id.clone(),
                        model: chosen.name.clone(),
                    });
                    continue;
                }

                // Step 4 + invariant 4/7: apply every budget as a hard
                // ceiling; excess is skipped(budget), never silently dropped.
                let is_external =
                    matches!(chosen.backend, Backend::Pi | Backend::Agy | Backend::Codex);
                let repo_count = *dispatch_count_by_repo.get(repo_name).unwrap_or(&0);
                let over_cycle_ceiling = global_dispatch_count >= budgets.max_dispatches_per_cycle;
                let over_repo_ceiling = repo_count >= budgets.max_active_per_repo;
                let over_external_ceiling =
                    is_external && global_external_count >= budgets.max_external_dispatches;

                if over_cycle_ceiling || over_repo_ceiling || over_external_ceiling {
                    plan.skips.push(Skip {
                        repo: repo_name.to_string(),
                        issue_id: issue.id.clone(),
                        reason: SkipCode::Budget,
                    });
                    continue;
                }

                let verify_cmd = routing
                    .verify_cmd
                    .clone()
                    .expect("auto_dispatch_eligible requires Some(verify_cmd)");
                plan.dispatches.push(Dispatch {
                    repo: repo_name.to_string(),
                    issue_id: issue.id.clone(),
                    model: chosen.name.clone(),
                    verify_cmd,
                });
                global_dispatch_count += 1;
                *dispatch_count_by_repo.entry(repo_name).or_insert(0) += 1;
                if is_external {
                    global_external_count += 1;
                }
                *dispatch_count_by_model
                    .entry(chosen.name.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    plan
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Freshness, ZeroState};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    // --- fixture builders ---

    fn issue(
        id: &str,
        priority: u32,
        created_at: &str,
        tier_floor: &str,
        complexity: &str,
        verify_cmd: Option<&str>,
    ) -> Issue {
        let mut metadata = BTreeMap::new();
        metadata.insert("tier_floor".to_string(), json!(tier_floor));
        metadata.insert("complexity".to_string(), json!(complexity));
        if let Some(cmd) = verify_cmd {
            metadata.insert("verify_cmd".to_string(), json!(cmd));
        }
        Issue {
            id: id.to_string(),
            title: format!("issue {id}"),
            description: String::new(),
            acceptance_criteria: String::new(),
            notes: String::new(),
            status: "open".to_string(),
            priority,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "fixture".to_string(),
            created_at: created_at.to_string(),
            created_by: "fixture".to_string(),
            updated_at: created_at.to_string(),
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

    fn untriaged_issue(id: &str, priority: u32, created_at: &str) -> Issue {
        let mut i = issue(id, priority, created_at, "junior", "S", None);
        i.metadata = None;
        i.notes = "no routing fields anywhere in here".to_string();
        i
    }

    fn issue_with_invalid_tier_floor(id: &str, priority: u32, created_at: &str) -> Issue {
        let mut metadata = BTreeMap::new();
        metadata.insert("tier_floor".to_string(), json!("boss"));
        metadata.insert("complexity".to_string(), json!("M"));
        let mut i = issue(id, priority, created_at, "senior", "M", None);
        i.metadata = Some(metadata);
        i
    }

    fn roster_entry(
        name: &str,
        tier: Tier,
        ceiling: Ceiling,
        efficiency: Efficiency,
        backend: Backend,
    ) -> RosterEntry {
        RosterEntry {
            name: name.to_string(),
            tier,
            ceiling,
            efficiency,
            backend,
            dispatch_id: format!("dispatch-{name}"),
            reasoning_effort: None,
            provider: String::new(),
            cost: Cost::Paid,
            fallback: Vec::new(),
        }
    }

    /// Roster entry with an explicit `cost` axis (defaults to Paid in
    /// `roster_entry`). Used by the cost-axis gate invariant tests.
    fn roster_entry_with_cost(
        name: &str,
        tier: Tier,
        ceiling: Ceiling,
        efficiency: Efficiency,
        backend: Backend,
        cost: Cost,
    ) -> RosterEntry {
        let mut e = roster_entry(name, tier, ceiling, efficiency, backend);
        e.cost = cost;
        e
    }

    /// Build a `CostPolicy` lookup map for one repo in tests.
    fn repo_policy_map(repo: &str, policy: CostPolicy) -> HashMap<String, CostPolicy> {
        let mut m = HashMap::new();
        m.insert(repo.to_string(), policy);
        m
    }

    /// Issue carrying the per-item `data_policy: trains-ok` opt-in, which
    /// lifts the `FreeTrainsInput` repo-policy gate for that one item.
    fn issue_with_trains_ok(
        id: &str,
        priority: u32,
        created_at: &str,
        tier_floor: &str,
        complexity: &str,
        verify_cmd: Option<&str>,
    ) -> Issue {
        let mut i = issue(id, priority, created_at, tier_floor, complexity, verify_cmd);
        let metadata = i.metadata.get_or_insert_with(BTreeMap::new);
        metadata.insert("data_policy".to_string(), json!("trains-ok"));
        i
    }

    fn active_repo(name: &str, ready: Vec<Issue>) -> RepoSnapshot {
        RepoSnapshot {
            path: PathBuf::from(format!("/fixtures/{name}")),
            name: name.to_string(),
            is_beads_repo: true,
            skip_reason: None,
            ready,
            count: 0,
            blocked: Vec::new(),
            zero_state: ZeroState::NotApplicable,
            freshness: Freshness::Unknown,
        }
    }

    fn skipped_repo(name: &str, reason: SkipReason, ready: Vec<Issue>) -> RepoSnapshot {
        RepoSnapshot {
            path: PathBuf::from(format!("/fixtures/{name}")),
            name: name.to_string(),
            is_beads_repo: true,
            skip_reason: Some(reason),
            ready,
            count: 0,
            blocked: Vec::new(),
            zero_state: ZeroState::NotApplicable,
            freshness: Freshness::Unknown,
        }
    }

    fn budgets(max_cycle: u32, max_repo: u32, max_external: u32) -> Budgets {
        Budgets {
            max_dispatches_per_cycle: max_cycle,
            max_active_per_repo: max_repo,
            max_external_dispatches: max_external,
            use_bursar: true,
            unknown_429_cooldown_mins: 15,
            item_wall_clock_mins: 45,
            cycle_wall_clock_mins: 90,
            authorized_legacy_run_ids: Vec::new(),
        }
    }

    fn generous_budgets() -> Budgets {
        budgets(100, 100, 100)
    }

    fn ratchet_unlocked(names: &[&str]) -> HashMap<String, RatchetState> {
        names
            .iter()
            .map(|n| ((*n).to_string(), RatchetState::Unlocked))
            .collect()
    }

    fn ratchet_none() -> HashMap<String, RatchetState> {
        HashMap::new()
    }

    // --- invariant 1: closed roster ---

    #[test]
    fn invariant_1_closed_roster_dispatch_and_proposal_models_are_always_from_roster() {
        let roster = vec![
            roster_entry(
                "model-a",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "model-b",
                Tier::Lead,
                Ceiling::L,
                Efficiency::Std,
                Backend::Claude,
            ),
        ];
        let repos = vec![active_repo(
            "repo1",
            vec![
                issue(
                    "senior-item",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                ),
                issue("lead-item", 2, "2026-01-02T00:00:00Z", "lead", "L", None),
            ],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        let known: Vec<&str> = roster.iter().map(|r| r.name.as_str()).collect();
        for d in &plan.dispatches {
            assert!(known.contains(&d.model.as_str()));
        }
        for p in &plan.proposals {
            assert!(known.contains(&p.model.as_str()));
        }
        assert!(!plan.dispatches.is_empty() || !plan.proposals.is_empty());
    }

    #[test]
    fn invariant_1_empty_roster_flags_every_triaged_item_over_ceiling_never_fallback() {
        let roster: Vec<RosterEntry> = Vec::new();
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "i1",
                1,
                "2026-01-01T00:00:00Z",
                "junior",
                "S",
                Some("cargo test"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.flags.len(), 1);
        assert!(matches!(plan.flags[0], Flag::OverCeiling { .. }));
    }

    // --- invariant 2: tier_floor is a hard gate ---

    #[test]
    fn invariant_2_tier_floor_is_a_hard_gate_never_routes_below_floor() {
        let roster = vec![
            roster_entry(
                "junior-model",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Agy,
            ),
            roster_entry(
                "senior-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
        ];
        let repos = vec![active_repo(
            "repo1",
            vec![issue("i1", 1, "2026-01-01T00:00:00Z", "senior", "S", None)],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].model, "senior-model");
    }

    #[test]
    fn invariant_2_unparseable_tier_floor_flags_not_guesses() {
        let roster = vec![roster_entry(
            "any-model",
            Tier::Lead,
            Ceiling::Xl,
            Efficiency::Heavy,
            Backend::Claude,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue_with_invalid_tier_floor(
                "bad-floor",
                1,
                "2026-01-01T00:00:00Z",
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.flags.len(), 1);
        match &plan.flags[0] {
            Flag::Untriaged { missing, .. } => {
                assert_eq!(missing, &vec![MissingField::TierFloor]);
            }
            other => panic!("expected Untriaged flag, got {other:?}"),
        }
    }

    // --- cost-axis gate (Phase 1: FreeTrainsInput eligibility) ---

    /// A `FreeTrainsInput` model is NOT eligible on a repo whose
    /// `cost_policy` is `Proprietary` (the default for any repo absent from
    /// `[[repo_policy]]`). The paid sibling with identical tier/ceiling/
    /// efficiency is chosen instead. Fail-closed: a free-train model never
    /// shadows a paid one on a proprietary repo.
    #[test]
    fn cost_gate_free_trains_input_excluded_on_proprietary_repo() {
        let roster = vec![
            roster_entry_with_cost(
                "free-train-junior",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Pi,
                Cost::FreeTrainsInput,
            ),
            roster_entry_with_cost(
                "paid-junior",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Pi,
                Cost::Paid,
            ),
        ];
        let repos = vec![active_repo(
            "proprietary-repo",
            vec![issue(
                "fan-out-item",
                1,
                "2026-01-01T00:00:00Z",
                "junior",
                "S",
                Some("echo ok"),
            )],
        )];
        // No repo_policy entry → defaults to Proprietary.
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["proprietary-repo"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].model, "paid-junior");
    }

    /// A `FreeTrainsInput` model IS eligible on a repo whose `cost_policy`
    /// is `Oss`. With identical tier/ceiling/efficiency, it is preferred
    /// over the paid sibling by roster order (free-train listed first).
    #[test]
    fn cost_gate_free_trains_input_allowed_on_oss_repo() {
        let roster = vec![
            roster_entry_with_cost(
                "free-train-junior",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Pi,
                Cost::FreeTrainsInput,
            ),
            roster_entry_with_cost(
                "paid-junior",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Pi,
                Cost::Paid,
            ),
        ];
        let repos = vec![active_repo(
            "oss-repo",
            vec![issue(
                "fan-out-item",
                1,
                "2026-01-01T00:00:00Z",
                "junior",
                "S",
                Some("echo ok"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["oss-repo"]),
            &repo_policy_map("oss-repo", CostPolicy::Oss),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].model, "free-train-junior");
    }

    /// A bead carrying `data_policy: trains-ok` lifts the gate per-item:
    /// a `FreeTrainsInput` model becomes eligible on a Proprietary repo
    /// (e.g. a public-dataset task on a closed codebase).
    #[test]
    fn cost_gate_trains_ok_bead_opts_in_on_proprietary_repo() {
        let roster = vec![roster_entry_with_cost(
            "free-train-junior",
            Tier::Junior,
            Ceiling::S,
            Efficiency::Lean,
            Backend::Pi,
            Cost::FreeTrainsInput,
        )];
        let repos = vec![active_repo(
            "proprietary-repo",
            vec![issue_with_trains_ok(
                "opted-in-item",
                1,
                "2026-01-01T00:00:00Z",
                "junior",
                "S",
                Some("echo ok"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["proprietary-repo"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].model, "free-train-junior");
    }

    /// A `Free` (no-training) model is eligible on every repo policy
    /// including Proprietary. The cost gate only excludes
    /// `FreeTrainsInput`; `Free` and `Paid` are always eligible.
    #[test]
    fn cost_gate_free_no_train_allowed_on_proprietary_repo() {
        let roster = vec![roster_entry_with_cost(
            "free-junior",
            Tier::Junior,
            Ceiling::S,
            Efficiency::Lean,
            Backend::Pi,
            Cost::Free,
        )];
        let repos = vec![active_repo(
            "proprietary-repo",
            vec![issue(
                "fan-out-item",
                1,
                "2026-01-01T00:00:00Z",
                "junior",
                "S",
                Some("echo ok"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["proprietary-repo"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].model, "free-junior");
    }

    // --- invariant 3: fail closed everywhere ---

    #[test]
    fn invariant_3_missing_verify_cmd_is_not_dispatchable_falls_back_to_proposal() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue("i1", 1, "2026-01-01T00:00:00Z", "senior", "M", None)],
        )];
        // Fully qualified otherwise: senior tier_floor, ratchet unlocked, wide budgets.
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert!(
            plan.dispatches.is_empty(),
            "missing verify_cmd must never dispatch"
        );
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].model, "senior-model");
    }

    // --- invariant 4: one writer per repo ---

    #[test]
    fn invariant_4_in_progress_repo_is_skipped_entirely() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![skipped_repo(
            "busy-repo",
            SkipReason::InProgress,
            vec![
                issue(
                    "i1",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                ),
                issue(
                    "i2",
                    2,
                    "2026-01-02T00:00:00Z",
                    "junior",
                    "S",
                    Some("cargo test"),
                ),
            ],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["busy-repo"]),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert!(plan.proposals.is_empty());
        assert!(plan.flags.is_empty());
        assert_eq!(plan.skips.len(), 2);
        for s in &plan.skips {
            assert_eq!(s.reason, SkipCode::InProgress);
        }
    }

    #[test]
    fn invariant_4_max_one_active_dispatch_per_repo_per_cycle() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![
                issue(
                    "i1",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test 1"),
                ),
                issue(
                    "i2",
                    2,
                    "2026-01-02T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test 2"),
                ),
            ],
        )];
        let b = budgets(100, 1, 100); // max_active_per_repo = 1 (spec default)
        let plan = route(
            &repos,
            &roster,
            &b,
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].issue_id, "i1");
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].issue_id, "i2");
        assert_eq!(plan.skips[0].reason, SkipCode::Budget);
    }

    // --- invariant 5: never dispatch a personal chezmoi repo (defense in depth) ---

    #[test]
    fn invariant_5_excluded_personal_chezmoi_repo_is_never_dispatched() {
        let roster = vec![roster_entry(
            "any-model",
            Tier::Lead,
            Ceiling::Xl,
            Efficiency::Heavy,
            Backend::Claude,
        )];
        let repos = vec![skipped_repo(
            "chezmoi-personal",
            SkipReason::Excluded,
            vec![issue(
                "sneaky",
                1,
                "2026-01-01T00:00:00Z",
                "lead",
                "XL",
                Some("cargo test"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["chezmoi-personal"]),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].reason, SkipCode::Excluded);
    }

    // --- invariant 6: close only verified (dispatch always carries verify_cmd) ---

    #[test]
    fn invariant_6_every_dispatch_carries_the_verify_cmd_precondition_for_close_only_verified() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "i1",
                1,
                "2026-01-01T00:00:00Z",
                "senior",
                "M",
                Some("cargo test triage"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.dispatches[0].verify_cmd, "cargo test triage");
    }

    // --- invariant 7: budgets are ceilings, not targets ---

    #[test]
    fn invariant_7_budgets_are_hard_ceilings_excess_is_skipped_with_reason() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![
            active_repo(
                "repo1",
                vec![issue(
                    "i1",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                )],
            ),
            active_repo(
                "repo2",
                vec![issue(
                    "i2",
                    2,
                    "2026-01-02T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                )],
            ),
            active_repo(
                "repo3",
                vec![issue(
                    "i3",
                    3,
                    "2026-01-03T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                )],
            ),
        ];
        let b = budgets(2, 100, 100); // exactly-at-ceiling boundary = 2
        let ratchet = ratchet_unlocked(&["repo1", "repo2", "repo3"]);
        let plan = route(&repos, &roster, &b, &ratchet, &HashMap::new());
        assert_eq!(
            plan.dispatches.len(),
            2,
            "exactly at ceiling must be allowed"
        );
        assert_eq!(plan.skips.len(), 1, "excess beyond ceiling must be skipped");
        assert_eq!(plan.skips[0].issue_id, "i3");
        assert_eq!(plan.skips[0].reason, SkipCode::Budget);
    }

    #[test]
    fn invariant_7_max_external_dispatches_ceiling_caps_pi_agy_and_codex_combined() {
        let roster = vec![
            roster_entry(
                "pi-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "agy-model",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Agy,
            ),
            roster_entry(
                "codex-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Codex,
            ),
        ];
        let repos = vec![
            active_repo(
                "repo1",
                vec![issue(
                    "i1",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
            active_repo(
                "repo2",
                vec![issue(
                    "i2",
                    2,
                    "2026-01-02T00:00:00Z",
                    "junior",
                    "S",
                    Some("t"),
                )],
            ),
            active_repo(
                "repo3",
                vec![issue(
                    "i3",
                    3,
                    "2026-01-03T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
        ];
        let b = budgets(100, 100, 1); // max_external_dispatches = 1 (pi + agy + codex)
        let plan = route(
            &repos,
            &roster,
            &b,
            &ratchet_unlocked(&["repo1", "repo2", "repo3"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(plan.skips.len(), 2);
        assert_eq!(plan.skips[0].reason, SkipCode::Budget);
    }

    #[test]
    fn invariant_7_external_cap_does_not_block_subsequent_internal_backend_dispatch() {
        // With max_external_dispatches exhausted by pi/agy (external) items, a
        // subsequent claude-backend (internal) item that is still within the
        // per-cycle/per-repo budgets STILL dispatches — the external cap gates
        // only external backends, per-item, not a global halt on dispatching.
        let roster = vec![
            roster_entry(
                "pi-senior",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "claude-senior",
                Tier::Senior,
                Ceiling::L,
                Efficiency::Std,
                Backend::Claude,
            ),
        ];
        let repos = vec![
            // Processed first (priority 1): senior-M routes to the lean
            // pi-senior model (external), exhausting max_external_dispatches.
            active_repo(
                "repo-ext",
                vec![issue(
                    "ext-item",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                )],
            ),
            // Processed second (priority 2): senior-L routes to claude-senior
            // (internal — pi-senior's M ceiling can't cover complexity L).
            // Despite the external cap being hit, this internal item must
            // still dispatch.
            active_repo(
                "repo-int",
                vec![issue(
                    "int-item",
                    2,
                    "2026-01-02T00:00:00Z",
                    "senior",
                    "L",
                    Some("cargo test"),
                )],
            ),
        ];
        let b = budgets(100, 100, 1); // external cap = 1, exhausted by ext-item
        let plan = route(
            &repos,
            &roster,
            &b,
            &ratchet_unlocked(&["repo-ext", "repo-int"]),
            &HashMap::new(),
        );
        assert_eq!(
            plan.dispatches.len(),
            2,
            "internal item must dispatch despite an exhausted external cap"
        );
        assert_eq!(plan.dispatches[0].issue_id, "ext-item");
        assert_eq!(plan.dispatches[0].model, "pi-senior");
        assert_eq!(plan.dispatches[1].issue_id, "int-item");
        assert_eq!(plan.dispatches[1].model, "claude-senior");
        assert!(
            plan.skips.is_empty(),
            "no skip expected — both items are within their relevant budgets"
        );
    }

    // --- invariant 8: no silent drops ---

    fn assert_every_ready_item_lands_in_one_bucket(repos: &[RepoSnapshot], plan: &Plan) {
        let mut all_ids: Vec<String> = Vec::new();
        for r in repos {
            for i in &r.ready {
                all_ids.push(i.id.clone());
            }
        }

        let mut seen: Vec<String> = Vec::new();
        for d in &plan.dispatches {
            seen.push(d.issue_id.clone());
        }
        for p in &plan.proposals {
            seen.push(p.issue_id.clone());
        }
        for f in &plan.flags {
            match f {
                Flag::Untriaged { issue_id, .. } | Flag::OverCeiling { issue_id, .. } => {
                    seen.push(issue_id.clone());
                }
                Flag::ScanGap { .. } | Flag::RosterDrift => {}
            }
        }
        for s in &plan.skips {
            seen.push(s.issue_id.clone());
        }

        all_ids.sort();
        seen.sort();
        assert_eq!(
            all_ids, seen,
            "every ready item must land in exactly one output bucket, exactly once"
        );
    }

    #[test]
    fn invariant_8_no_silent_drops_every_ready_item_lands_in_exactly_one_bucket() {
        let roster = vec![
            roster_entry(
                "junior-model",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Agy,
            ),
            roster_entry(
                "senior-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "lead-model",
                Tier::Lead,
                Ceiling::L,
                Efficiency::Std,
                Backend::Claude,
            ),
        ];
        let repos = vec![
            active_repo(
                "repo1",
                vec![
                    issue(
                        "dispatched",
                        1,
                        "2026-01-01T00:00:00Z",
                        "senior",
                        "M",
                        Some("cargo test"),
                    ),
                    issue(
                        "proposed-lead",
                        2,
                        "2026-01-02T00:00:00Z",
                        "lead",
                        "L",
                        Some("cargo test"),
                    ),
                    untriaged_issue("untriaged", 3, "2026-01-03T00:00:00Z"),
                    issue(
                        "over-ceiling",
                        4,
                        "2026-01-04T00:00:00Z",
                        "junior",
                        "XL",
                        Some("cargo test"),
                    ),
                ],
            ),
            active_repo(
                "repo2",
                vec![issue(
                    "budget-excess",
                    5,
                    "2026-01-05T00:00:00Z",
                    "junior",
                    "S",
                    Some("cargo test"),
                )],
            ),
            skipped_repo(
                "repo3-busy",
                SkipReason::InProgress,
                vec![issue(
                    "in-progress-1",
                    6,
                    "2026-01-06T00:00:00Z",
                    "senior",
                    "M",
                    Some("cargo test"),
                )],
            ),
            skipped_repo("chezmoi-config", SkipReason::Excluded, vec![]),
        ];
        let b = budgets(1, 100, 100); // only 1 global dispatch allowed this cycle
        let ratchet = ratchet_unlocked(&["repo1", "repo2", "repo3-busy"]);

        let plan = route(&repos, &roster, &b, &ratchet, &HashMap::new());

        assert_every_ready_item_lands_in_one_bucket(&repos, &plan);
    }

    // --- invariant 9: ratchet failure re-locks (locked repo always proposes) ---

    #[test]
    fn invariant_9_locked_ratchet_forces_proposal_never_auto_dispatch() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "i1",
                1,
                "2026-01-01T00:00:00Z",
                "senior",
                "M",
                Some("cargo test"),
            )],
        )];
        // Fully qualified otherwise (senior tier, verify_cmd present, wide
        // budgets) but the repo's ratchet has not earned unlock (e.g.
        // re-locked to 0 after a prior rejected proposal or failed verify,
        // per invariant 9) -> must propose only.
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].model, "senior-model");
    }

    #[test]
    fn invariant_9_unlocked_ratchet_permits_auto_dispatch_once_fully_qualified() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "i1",
                1,
                "2026-01-01T00:00:00Z",
                "senior",
                "M",
                Some("cargo test"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert!(plan.proposals.is_empty());
    }

    // --- routing step 5: lead-floor items are ALWAYS propose-only ---

    #[test]
    fn lead_floor_item_never_auto_dispatches_even_when_fully_qualified() {
        // Routing algorithm step 5 (and the ratchet safety property): a
        // lead-floor item is ALWAYS a proposal, never an auto-dispatch — even
        // with a runnable verify_cmd, an unlocked ratchet, and budget room.
        // Regression test for the `routing.tier_floor != Tier::Lead` guard in
        // `route`; no other test isolates it (removing that clause leaves the
        // entire suite green — verified by mutation).
        let roster = vec![roster_entry(
            "lead-model",
            Tier::Lead,
            Ceiling::L,
            Efficiency::Std,
            Backend::Claude,
        )];
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "lead-item",
                1,
                "2026-01-01T00:00:00Z",
                "lead",
                "L",
                Some("cargo test"),
            )],
        )];
        // Fully auto-dispatch-qualified in every respect EXCEPT the lead floor.
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert!(
            plan.dispatches.is_empty(),
            "lead-floor items must never auto-dispatch (routing step 5)"
        );
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].model, "lead-model");
        assert!(plan.skips.is_empty());
        assert!(plan.flags.is_empty());
    }

    // --- candidate-selection shape: lowest qualifying tier, then efficiency, then tie-breaks ---

    #[test]
    fn lowest_qualifying_tier_is_preferred_over_higher_tiers() {
        let roster = vec![
            roster_entry(
                "lead-model",
                Tier::Lead,
                Ceiling::Xl,
                Efficiency::Heavy,
                Backend::Claude,
            ),
            roster_entry(
                "senior-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Std,
                Backend::Pi,
            ),
            roster_entry(
                "junior-model",
                Tier::Junior,
                Ceiling::S,
                Efficiency::Lean,
                Backend::Agy,
            ),
        ];
        // tier_floor junior, complexity S: all three roster entries qualify on
        // ceiling, but the lowest qualifying tier (junior) must win even
        // though it's listed last in the roster.
        let repos = vec![active_repo(
            "repo1",
            vec![issue("i1", 1, "2026-01-01T00:00:00Z", "junior", "S", None)],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].model, "junior-model");
    }

    #[test]
    fn most_efficient_model_is_preferred_within_the_same_qualifying_tier() {
        let roster = vec![
            roster_entry(
                "heavy-senior",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Heavy,
                Backend::Pi,
            ),
            roster_entry(
                "lean-senior",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
        ];
        let repos = vec![active_repo(
            "repo1",
            vec![issue("i1", 1, "2026-01-01T00:00:00Z", "senior", "M", None)],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        assert_eq!(plan.proposals[0].model, "lean-senior");
    }

    #[test]
    fn tie_break_equal_efficiency_and_equal_dispatch_count_falls_back_to_roster_order() {
        let roster = vec![
            roster_entry(
                "first-listed",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "second-listed",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
        ];
        let repos = vec![active_repo(
            "repo1",
            vec![issue("i1", 1, "2026-01-01T00:00:00Z", "senior", "M", None)],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        assert_eq!(plan.proposals[0].model, "first-listed");
    }

    #[test]
    fn tie_break_prefers_fewest_dispatches_so_far_this_cycle_over_roster_order() {
        let roster = vec![
            roster_entry(
                "first-listed",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "second-listed",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
        ];
        // Two items in two different unlocked repos so each independently
        // dispatches; after "first-listed" gets the first dispatch, the
        // second item's tie-break must prefer "second-listed" (fewer
        // dispatches so far), even though "first-listed" is roster-first.
        let repos = vec![
            active_repo(
                "repo1",
                vec![issue(
                    "i1",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
            active_repo(
                "repo2",
                vec![issue(
                    "i2",
                    2,
                    "2026-01-02T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
        ];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1", "repo2"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 2);
        assert_eq!(plan.dispatches[0].model, "first-listed");
        assert_eq!(plan.dispatches[1].model, "second-listed");
    }

    #[test]
    fn over_ceiling_flag_when_complexity_exceeds_every_qualifying_ceiling() {
        let roster = vec![
            roster_entry(
                "senior-model",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                Backend::Pi,
            ),
            roster_entry(
                "lead-model",
                Tier::Lead,
                Ceiling::L,
                Efficiency::Std,
                Backend::Claude,
            ),
        ];
        // complexity XL, but the highest ceiling anywhere in the roster is L.
        let repos = vec![active_repo(
            "repo1",
            vec![issue(
                "i1",
                1,
                "2026-01-01T00:00:00Z",
                "senior",
                "XL",
                Some("t"),
            )],
        )];
        let plan = route(
            &repos,
            &roster,
            &generous_budgets(),
            &ratchet_unlocked(&["repo1"]),
            &HashMap::new(),
        );
        assert!(plan.dispatches.is_empty());
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.flags.len(), 1);
        match &plan.flags[0] {
            Flag::OverCeiling { complexity, .. } => assert_eq!(*complexity, Ceiling::Xl),
            other => panic!("expected OverCeiling flag, got {other:?}"),
        }
    }

    #[test]
    fn priority_then_created_at_ordering_determines_processing_order_for_budget_ceilings() {
        let roster = vec![roster_entry(
            "senior-model",
            Tier::Senior,
            Ceiling::M,
            Efficiency::Lean,
            Backend::Pi,
        )];
        // Same priority; ordering must fall back to created_at ascending
        // (oldest first).
        let repos = vec![
            active_repo(
                "repo-newer",
                vec![issue(
                    "newer",
                    1,
                    "2026-01-02T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
            active_repo(
                "repo-older",
                vec![issue(
                    "older",
                    1,
                    "2026-01-01T00:00:00Z",
                    "senior",
                    "M",
                    Some("t"),
                )],
            ),
        ];
        let b = budgets(1, 100, 100);
        let plan = route(
            &repos,
            &roster,
            &b,
            &ratchet_unlocked(&["repo-newer", "repo-older"]),
            &HashMap::new(),
        );
        assert_eq!(plan.dispatches.len(), 1);
        assert_eq!(
            plan.dispatches[0].issue_id, "older",
            "oldest created_at must be processed first under a tight budget"
        );
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].issue_id, "newer");
    }

    // --- real roster sanity check (guards against roster drift silently changing routing) ---

    #[test]
    #[ignore = "Bursar owns the live roster after conductor-bursar-roster cutover"]
    fn real_roster_routes_the_specs_tesela_headline_fixture_to_a_lean_senior_model() {
        let cfg = crate::config::parse_str(include_str!("../conductor.toml"))
            .expect("checked-in config parses");
        let notes = "tier_floor: senior · complexity: S-M · verify_type: wrangler dev + cargo test";
        let mut i = issue(
            "tesela-headline",
            1,
            "2026-01-01T00:00:00Z",
            "senior",
            "M",
            None,
        );
        i.metadata = None;
        i.notes = notes.to_string();
        let repos = vec![active_repo("tesela", vec![i])];
        let plan = route(
            &repos,
            &cfg.roster,
            &generous_budgets(),
            &ratchet_none(),
            &HashMap::new(),
        );
        // No runnable verify_cmd (verify_type is prose, not a command) and a
        // locked ratchet -> proposal, not dispatch.
        assert_eq!(plan.proposals.len(), 1);
        let model = &plan.proposals[0].model;
        let entry = cfg
            .roster
            .iter()
            .find(|r| &r.name == model)
            .expect("chosen model must be in the roster");
        assert_eq!(entry.tier, Tier::Senior);
        assert_eq!(entry.efficiency, Efficiency::Lean);
    }
}
