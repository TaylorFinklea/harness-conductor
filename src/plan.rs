//! cycle plan build/serialize (~/.local/state/conductor/plans/<cycle-id>.json)

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bd::Issue;
use crate::config::{Ceiling, Tier};
use crate::fields::RoutingFields;
use crate::route::{CandidateAudit, RouteAdvice, RouteCandidate};
use crate::triage::{Flag, Plan, SkipCode};

/// Serializable cycle plan written to the state dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CyclePlan {
    pub(crate) cycle_id: String,
    pub(crate) created_at: String,
    pub(crate) dispatches: Vec<DispatchEntry>,
    pub(crate) proposals: Vec<ProposalEntry>,
    pub(crate) flags: Vec<FlagEntry>,
    pub(crate) skips: Vec<SkipEntry>,
    #[serde(default)]
    pub(crate) provider_routes: Vec<ProviderRouteRecord>,
    pub(crate) approval_scope: ApprovalScope,
    pub(crate) item_authorizations: Vec<ItemAuthorizationRecord>,
}

/// Immutable maximum blast radius displayed before cycle approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApprovalScope {
    pub(crate) kind: ApprovalScopeKind,
    pub(crate) selectors: Vec<ScopeSelector>,
    pub(crate) repo_paths: Vec<String>,
    pub(crate) max_dispatch_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ApprovalScopeKind {
    FleetAudit,
    RepositoryScope,
    ExactItemScope,
}

impl ApprovalScopeKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FleetAudit => "fleet-audit",
            Self::RepositoryScope => "repository-scope",
            Self::ExactItemScope => "exact-item-scope",
        }
    }
}

/// Canonical user selector retained in the immutable cycle plan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum ScopeSelector {
    Repository { repo: String },
    ExactItem { repo: String, issue_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ItemAuthorizationRecord {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeError(String);

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeError {}

/// Approval-time route and provider evidence for one proposed or dispatched item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProviderRouteRecord {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) selected_model: Option<String>,
    pub(crate) approved_models: Vec<String>,
    pub(crate) candidates: Vec<ProviderCandidateRecord>,
    pub(crate) terminal_defer: bool,
}

/// Complete audit row for one roster candidate at approval time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProviderCandidateRecord {
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) backend: String,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) availability: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) checked_at: Option<String>,
    pub(crate) data_as_of: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) expiry_basis: Option<String>,
    pub(crate) action: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) outcome: String,
    pub(crate) routing_reasons: Vec<CandidateReasonRecord>,
    pub(crate) exclusion_reasons: Vec<CandidateReasonRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CandidateReasonRecord {
    pub(crate) code: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DispatchEntry {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
    pub(crate) verify_cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProposalEntry {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FlagEntry {
    pub(crate) kind: String,
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SkipEntry {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) reason: String,
}

impl Default for ApprovalScope {
    fn default() -> Self {
        Self {
            kind: ApprovalScopeKind::FleetAudit,
            selectors: Vec::new(),
            repo_paths: Vec::new(),
            max_dispatch_count: 0,
        }
    }
}

impl ApprovalScope {
    pub(crate) fn new(
        kind: ApprovalScopeKind,
        mut selectors: Vec<ScopeSelector>,
        mut repo_paths: Vec<String>,
        max_dispatch_count: usize,
    ) -> Result<Self, ScopeError> {
        match kind {
            ApprovalScopeKind::FleetAudit if !selectors.is_empty() => {
                return Err(ScopeError(
                    "fleet-audit scope cannot carry explicit selectors".to_string(),
                ));
            }
            ApprovalScopeKind::RepositoryScope
                if selectors.is_empty()
                    || selectors
                        .iter()
                        .any(|selector| !matches!(selector, ScopeSelector::Repository { .. })) =>
            {
                return Err(ScopeError(
                    "repository-scope requires repository selectors".to_string(),
                ));
            }
            ApprovalScopeKind::ExactItemScope
                if selectors.is_empty()
                    || selectors
                        .iter()
                        .any(|selector| !matches!(selector, ScopeSelector::ExactItem { .. })) =>
            {
                return Err(ScopeError(
                    "exact-item-scope requires exact item selectors".to_string(),
                ));
            }
            ApprovalScopeKind::FleetAudit
            | ApprovalScopeKind::RepositoryScope
            | ApprovalScopeKind::ExactItemScope => {}
        }
        if !matches!(kind, ApprovalScopeKind::FleetAudit) && repo_paths.is_empty() {
            return Err(ScopeError(
                "explicit approval scope requires canonical repository paths".to_string(),
            ));
        }
        reject_duplicate_selectors(&selectors)?;
        reject_duplicate_repo_paths(&repo_paths)?;
        if !matches!(kind, ApprovalScopeKind::FleetAudit) && max_dispatch_count == 0 {
            return Err(ScopeError(
                "explicit approval scope must authorize at least one visible item".to_string(),
            ));
        }
        for selector in &selectors {
            let repo = match selector {
                ScopeSelector::Repository { repo } | ScopeSelector::ExactItem { repo, .. } => repo,
            };
            if !repo_paths.contains(repo) {
                return Err(ScopeError(format!(
                    "scope selector repository {repo} is not a persisted canonical path"
                )));
            }
        }
        selectors.sort();
        repo_paths.sort();
        Ok(Self {
            kind,
            selectors,
            repo_paths,
            max_dispatch_count,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ScopeError> {
        let normalized = Self::new(
            self.kind,
            self.selectors.clone(),
            self.repo_paths.clone(),
            self.max_dispatch_count,
        )?;
        if normalized != *self {
            return Err(ScopeError(
                "approval scope selectors and paths are not canonically ordered".to_string(),
            ));
        }
        Ok(())
    }
}

fn reject_duplicate_selectors(selectors: &[ScopeSelector]) -> Result<(), ScopeError> {
    let mut canonical = selectors.to_vec();
    canonical.sort();
    if let Some(duplicate) = canonical.windows(2).find_map(|pair| {
        (pair[0] == pair[1]).then(|| serde_json::to_string(&pair[0]).unwrap_or_default())
    }) {
        return Err(ScopeError(format!("duplicate scope selector {duplicate}")));
    }
    Ok(())
}

fn reject_duplicate_repo_paths(repo_paths: &[String]) -> Result<(), ScopeError> {
    let mut canonical = repo_paths.to_vec();
    canonical.sort();
    if let Some(duplicate) = canonical
        .windows(2)
        .find_map(|pair| (pair[0] == pair[1]).then(|| pair[0].clone()))
    {
        return Err(ScopeError(format!(
            "duplicate canonical repository path {duplicate}"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct AuthorizationInput<'a> {
    schema: &'static str,
    repo_path: &'a str,
    issue_id: &'a str,
    title: &'a str,
    description: &'a str,
    acceptance_criteria: &'a str,
    routing: AuthorizationRouting<'a>,
    verify_cmd: Option<&'a str>,
    selected_model: &'a str,
    approved_models: &'a [String],
}

#[derive(Serialize)]
struct AuthorizationRouting<'a> {
    tier_floor: &'a str,
    complexity: &'a str,
    trains_ok: bool,
}

/// Hashes only the inputs authorized by the approval report. Callers must pass
/// a canonical absolute repository path.
pub(crate) fn item_authorization_hash(
    repo_path: &str,
    issue: &Issue,
    routing: &RoutingFields,
    selected_model: &str,
    approved_models: &[String],
) -> Result<String, AuthorizationError> {
    if approved_models.first().map(String::as_str) != Some(selected_model) {
        return Err(AuthorizationError::InvalidEnvelope(format!(
            "approved envelope must start with selected model {selected_model}"
        )));
    }
    let mut unique = approved_models.to_vec();
    unique.sort();
    if unique.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(AuthorizationError::InvalidEnvelope(
            "approved envelope contains a duplicate model".to_string(),
        ));
    }
    let input = AuthorizationInput {
        schema: "conductor-item-authorization-v1",
        repo_path,
        issue_id: &issue.id,
        title: &issue.title,
        description: &issue.description,
        acceptance_criteria: &issue.acceptance_criteria,
        routing: AuthorizationRouting {
            tier_floor: tier_label(routing.tier_floor),
            complexity: complexity_label(routing.complexity),
            trains_ok: routing.trains_ok,
        },
        verify_cmd: routing.verify_cmd.as_deref(),
        selected_model,
        approved_models,
    };
    let digest = Sha256::digest(serde_json::to_vec(&input)?);
    Ok(format!("{digest:x}"))
}

#[derive(Debug)]
pub(crate) enum AuthorizationError {
    InvalidEnvelope(String),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvelope(message) => f.write_str(message),
            Self::Serialize(error) => write!(f, "authorization serialization failed: {error}"),
        }
    }
}

impl std::error::Error for AuthorizationError {}

impl From<serde_json::Error> for AuthorizationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

const fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Lead => "lead",
        Tier::Senior => "senior",
        Tier::Junior => "junior",
    }
}

const fn complexity_label(complexity: Ceiling) -> &'static str {
    match complexity {
        Ceiling::S => "S",
        Ceiling::M => "M",
        Ceiling::L => "L",
        Ceiling::Xl => "XL",
    }
}

impl CyclePlan {
    /// Builds a serializable plan from the triage output.
    pub(crate) fn from_triage(cycle_id: &str, created_at: &str, plan: &Plan) -> Self {
        let dispatches = plan
            .dispatches
            .iter()
            .map(|d| DispatchEntry {
                repo: d.repo.clone(),
                issue_id: d.issue_id.clone(),
                model: d.model.clone(),
                verify_cmd: d.verify_cmd.clone(),
            })
            .collect();
        let proposals = plan
            .proposals
            .iter()
            .map(|p| ProposalEntry {
                repo: p.repo.clone(),
                issue_id: p.issue_id.clone(),
                model: p.model.clone(),
            })
            .collect();
        let flags = plan.flags.iter().map(flag_entry).collect();
        let skips = plan
            .skips
            .iter()
            .map(|s| SkipEntry {
                repo: s.repo.clone(),
                issue_id: s.issue_id.clone(),
                reason: skip_code_str(s.reason).to_string(),
            })
            .collect();
        Self {
            cycle_id: cycle_id.to_string(),
            created_at: created_at.to_string(),
            dispatches,
            proposals,
            flags,
            skips,
            provider_routes: Vec::new(),
            approval_scope: ApprovalScope {
                max_dispatch_count: plan.dispatches.len(),
                ..ApprovalScope::default()
            },
            item_authorizations: Vec::new(),
        }
    }

    /// Replaces executable route choices with provider-aware advice and retains
    /// terminal defers as immutable approval evidence.
    pub(crate) fn apply_provider_routes(
        &mut self,
        advice: impl IntoIterator<Item = (String, RouteAdvice)>,
    ) {
        for (issue_id, advice) in advice {
            let record = ProviderRouteRecord::from_advice(issue_id, &advice);
            if let Some(selected) = record.selected_model.as_deref() {
                if let Some(dispatch) = self
                    .dispatches
                    .iter_mut()
                    .find(|entry| entry.repo == record.repo && entry.issue_id == record.issue_id)
                {
                    dispatch.model = selected.to_string();
                }
                if let Some(proposal) = self
                    .proposals
                    .iter_mut()
                    .find(|entry| entry.repo == record.repo && entry.issue_id == record.issue_id)
                {
                    proposal.model = selected.to_string();
                }
            } else {
                self.dispatches
                    .retain(|entry| entry.repo != record.repo || entry.issue_id != record.issue_id);
                self.proposals
                    .retain(|entry| entry.repo != record.repo || entry.issue_id != record.issue_id);
            }
            self.provider_routes.push(record);
        }
    }

    /// Writes the plan JSON to `<state_dir>/plans/<cycle-id>.json`.
    pub(crate) fn save(&self, state_dir: &Path) -> io::Result<PathBuf> {
        self.approval_scope.validate().map_err(io::Error::other)?;
        let plans_dir = state_dir.join("plans");
        std::fs::create_dir_all(&plans_dir)?;
        let path = plans_dir.join(format!("{}.json", self.cycle_id));
        let mut json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        json.push(b'\n');
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Loads `<state_dir>/plans/<cycle-id>.json`.
    pub(crate) fn load(state_dir: &Path, cycle_id: &str) -> io::Result<Self> {
        let path = state_dir.join("plans").join(format!("{cycle_id}.json"));
        let bytes = std::fs::read(path)?;
        let plan: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        plan.approval_scope.validate().map_err(io::Error::other)?;
        Ok(plan)
    }
}

impl ProviderRouteRecord {
    pub(crate) fn from_advice(issue_id: String, advice: &RouteAdvice) -> Self {
        let selected_model = advice
            .selected
            .as_ref()
            .map(|candidate| candidate.model.clone());
        let approved_models = advice
            .selected
            .iter()
            .chain(advice.approved_fallbacks.iter())
            .map(|candidate| candidate.model.clone())
            .collect();
        let candidates = advice
            .audit
            .iter()
            .map(|audit| {
                ProviderCandidateRecord::from_audit(
                    audit,
                    selected_model.as_deref(),
                    &advice.approved_fallbacks,
                )
            })
            .collect();
        Self {
            repo: advice.repo.clone(),
            issue_id,
            terminal_defer: selected_model.is_none(),
            selected_model,
            approved_models,
            candidates,
        }
    }
}

impl ProviderCandidateRecord {
    fn from_audit(
        audit: &CandidateAudit,
        selected_model: Option<&str>,
        approved_fallbacks: &[RouteCandidate],
    ) -> Self {
        let evidence = audit.candidate.evidence.as_ref();
        let outcome = if selected_model == Some(audit.model.as_str()) {
            "selected"
        } else if approved_fallbacks
            .iter()
            .any(|candidate| candidate.model == audit.model)
        {
            "approved-fallback"
        } else if audit.eligible {
            "eligible-unapproved"
        } else {
            "excluded"
        };
        let routing_reasons: Vec<CandidateReasonRecord> = audit
            .reasons
            .iter()
            .map(|reason| CandidateReasonRecord {
                code: reason.code.clone(),
                reason: reason.text.clone(),
            })
            .collect();
        Self {
            model: audit.model.clone(),
            provider: audit.candidate.provider.clone(),
            backend: format!("{:?}", audit.candidate.backend).to_ascii_lowercase(),
            dispatch_id: audit.candidate.dispatch_id.clone(),
            reasoning_effort: audit.candidate.reasoning_effort.clone(),
            availability: evidence
                .and_then(|value| value.availability)
                .map(|value| value.to_string()),
            source: evidence.and_then(|value| value.source.clone()),
            checked_at: evidence.and_then(|value| value.checked_at.clone()),
            data_as_of: evidence.and_then(|value| value.data_as_of.clone()),
            expires_at: evidence.and_then(|value| value.expires_at.clone()),
            expiry_basis: evidence.and_then(|value| value.expiry_basis.clone()),
            action: evidence.map(|value| value.action.label().to_string()),
            reason: evidence.map(|value| value.reason.clone()),
            outcome: outcome.to_string(),
            exclusion_reasons: if audit.eligible {
                Vec::new()
            } else {
                routing_reasons.clone()
            },
            routing_reasons,
        }
    }
}

fn flag_entry(flag: &Flag) -> FlagEntry {
    match flag {
        Flag::Untriaged {
            repo,
            issue_id,
            missing,
        } => FlagEntry {
            kind: "untriaged".to_string(),
            repo: repo.clone(),
            issue_id: issue_id.clone(),
            detail: format!(
                "missing: {}",
                missing
                    .iter()
                    .map(|m| match m {
                        crate::fields::MissingField::TierFloor => "tier_floor",
                        crate::fields::MissingField::Complexity => "complexity",
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        Flag::OverCeiling {
            repo,
            issue_id,
            complexity,
        } => FlagEntry {
            kind: "over-ceiling".to_string(),
            repo: repo.clone(),
            issue_id: issue_id.clone(),
            detail: format!("complexity {complexity:?} exceeds every qualifying model ceiling"),
        },
        Flag::ScanGap { repo, detail } => FlagEntry {
            kind: "scan-gap".to_string(),
            repo: repo.clone(),
            issue_id: String::new(),
            detail: detail.clone(),
        },
        Flag::RosterDrift => FlagEntry {
            kind: "roster-drift".to_string(),
            repo: String::new(),
            issue_id: String::new(),
            detail: "scorecard and conductor.toml disagree".to_string(),
        },
    }
}

fn skip_code_str(code: SkipCode) -> &'static str {
    match code {
        SkipCode::Excluded => "excluded",
        SkipCode::InProgress => "in-progress",
        SkipCode::NotBeadsRepo => "not-beads-repo",
        SkipCode::NotGitRepo => "not-git-repo",
        SkipCode::Budget => "budget",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::bursar::{Availability, BudgetAction};
    use crate::config::{Backend, Cost};
    use crate::route::{ProviderEvidence, RouteReason};
    use crate::triage::{Dispatch, Proposal, Skip};

    #[test]
    fn cycle_plan_serializes_and_saves() {
        let plan = Plan {
            dispatches: vec![Dispatch {
                repo: "r1".to_string(),
                issue_id: "i1".to_string(),
                model: "m1".to_string(),
                verify_cmd: "cargo test".to_string(),
            }],
            proposals: vec![Proposal {
                repo: "r2".to_string(),
                issue_id: "i2".to_string(),
                model: "m2".to_string(),
            }],
            flags: vec![],
            skips: vec![Skip {
                repo: "r3".to_string(),
                issue_id: "i3".to_string(),
                reason: SkipCode::Budget,
            }],
        };

        let cp = CyclePlan::from_triage("cycle-20260702-120000", "2026-07-02T12:00:00Z", &plan);
        assert_eq!(cp.dispatches.len(), 1);
        assert_eq!(cp.proposals.len(), 1);
        assert_eq!(cp.skips.len(), 1);
        assert_eq!(cp.skips[0].reason, "budget");

        let tmp = std::env::temp_dir().join("conductor-plan-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = cp.save(&tmp).unwrap();
        assert!(path.is_file());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["cycle_id"], "cycle-20260702-120000");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn provider_routes_update_selection_and_serialize_complete_audit() {
        let plan = Plan {
            proposals: vec![Proposal {
                repo: "repo".to_string(),
                issue_id: "issue".to_string(),
                model: "legacy-choice".to_string(),
            }],
            ..Plan::default()
        };
        let evidence = ProviderEvidence {
            provider: "codex".to_string(),
            model: Some("observed-gpt".to_string()),
            availability: Some(Availability::Healthy),
            source: Some("bursar-api".to_string()),
            checked_at: Some("2026-07-13T12:00:00Z".to_string()),
            data_as_of: Some("2026-07-13T11:59:00Z".to_string()),
            expires_at: Some("2026-07-13T12:15:00Z".to_string()),
            expiry_basis: Some("human-override".to_string()),
            action: BudgetAction::Proceed,
            reason: "codex: proceed — healthy".to_string(),
        };
        let selected = RouteCandidate {
            model: "selected".to_string(),
            backend: Backend::Codex,
            dispatch_id: "gpt-5.6-luna".to_string(),
            reasoning_effort: Some("medium".to_string()),
            provider: "codex".to_string(),
            cost: Cost::Paid,
            fallback: Vec::new(),
            evidence: Some(evidence),
        };
        let audit = CandidateAudit {
            model: selected.model.clone(),
            eligible: true,
            candidate: selected.clone(),
            reasons: vec![RouteReason {
                code: "selected-current-algorithm".to_string(),
                text: "selected by provider-aware ordering".to_string(),
                rejection: None,
            }],
        };
        let advice = RouteAdvice {
            repo: "repo".to_string(),
            dispatch_excluded: false,
            intent: None,
            selected: Some(selected),
            alternatives: Vec::new(),
            approved_fallbacks: Vec::new(),
            audit: vec![audit],
        };

        let mut cycle = CyclePlan::from_triage("cycle", "2026-07-13T12:00:00Z", &plan);
        cycle.apply_provider_routes([("issue".to_string(), advice)]);

        assert_eq!(cycle.proposals[0].model, "selected");
        assert_eq!(cycle.provider_routes[0].approved_models, ["selected"]);
        let candidate = &cycle.provider_routes[0].candidates[0];
        assert_eq!(candidate.backend, "codex");
        assert_eq!(candidate.availability.as_deref(), Some("healthy"));
        assert_eq!(candidate.expiry_basis.as_deref(), Some("human-override"));
        assert_eq!(candidate.action.as_deref(), Some("proceed"));
        assert_eq!(candidate.outcome, "selected");
        assert!(candidate.exclusion_reasons.is_empty());

        let json = serde_json::to_value(&cycle).unwrap();
        assert_eq!(json["provider_routes"][0]["selected_model"], "selected");
        assert_eq!(
            json["provider_routes"][0]["candidates"][0]["checked_at"],
            "2026-07-13T12:00:00Z"
        );
    }

    #[test]
    fn approval_scope_serialization_is_canonical_and_rejects_duplicates() {
        let selectors = vec![
            ScopeSelector::Repository {
                repo: "/repos/bravo".to_string(),
            },
            ScopeSelector::Repository {
                repo: "/repos/alpha".to_string(),
            },
        ];
        let first = ApprovalScope::new(
            ApprovalScopeKind::RepositoryScope,
            selectors.clone(),
            vec!["/repos/bravo".to_string(), "/repos/alpha".to_string()],
            2,
        )
        .expect("scope");
        let second = ApprovalScope::new(
            ApprovalScopeKind::RepositoryScope,
            selectors.into_iter().rev().collect(),
            vec!["/repos/alpha".to_string(), "/repos/bravo".to_string()],
            2,
        )
        .expect("scope");
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );

        let duplicate_selector = vec![
            ScopeSelector::ExactItem {
                repo: "/repos/alpha".to_string(),
                issue_id: "a-1".to_string(),
            },
            ScopeSelector::ExactItem {
                repo: "/repos/alpha".to_string(),
                issue_id: "a-1".to_string(),
            },
        ];
        assert!(
            ApprovalScope::new(
                ApprovalScopeKind::ExactItemScope,
                duplicate_selector,
                vec!["/repos/alpha".to_string()],
                1,
            )
            .is_err()
        );
        assert!(
            ApprovalScope::new(
                ApprovalScopeKind::RepositoryScope,
                vec![ScopeSelector::Repository {
                    repo: "/repos/alpha".to_string(),
                }],
                vec!["/repos/alpha".to_string(), "/repos/alpha".to_string()],
                1,
            )
            .is_err()
        );
        assert!(
            ApprovalScope::new(
                ApprovalScopeKind::RepositoryScope,
                vec![ScopeSelector::Repository {
                    repo: "/repos/bravo".to_string(),
                }],
                vec!["/repos/alpha".to_string()],
                1,
            )
            .is_err()
        );
        assert!(
            ApprovalScope::new(
                ApprovalScopeKind::ExactItemScope,
                Vec::new(),
                vec!["/repos/alpha".to_string()],
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn authorization_hash_tracks_only_launch_authorization_inputs() {
        let issue = authorization_issue();
        let routing = RoutingFields {
            tier_floor: Tier::Senior,
            complexity: Ceiling::M,
            verify_cmd: Some("cargo test".to_string()),
            trains_ok: false,
        };
        let envelope = vec![
            "selected".to_string(),
            "fallback-a".to_string(),
            "fallback-b".to_string(),
        ];
        let baseline =
            item_authorization_hash("/repos/alpha", &issue, &routing, "selected", &envelope)
                .expect("hash");
        assert_eq!(
            baseline,
            "c0491080e93873b421a1dc1437fc02eeafdf5dddee39d5a5dc535bfec3a3fb73"
        );
        assert_eq!(
            baseline,
            item_authorization_hash("/repos/alpha", &issue, &routing, "selected", &envelope,)
                .expect("stable hash")
        );

        let mut changed_issue = issue.clone();
        changed_issue.title.push_str(" changed");
        assert_ne!(
            baseline,
            item_authorization_hash(
                "/repos/alpha",
                &changed_issue,
                &routing,
                "selected",
                &envelope,
            )
            .unwrap()
        );
        let mut changed_routing = routing.clone();
        changed_routing.verify_cmd = Some("cargo test --all".to_string());
        assert_ne!(
            baseline,
            item_authorization_hash(
                "/repos/alpha",
                &issue,
                &changed_routing,
                "selected",
                &envelope,
            )
            .unwrap()
        );
        let reordered_envelope = vec![
            "selected".to_string(),
            "fallback-b".to_string(),
            "fallback-a".to_string(),
        ];
        assert_ne!(
            baseline,
            item_authorization_hash(
                "/repos/alpha",
                &issue,
                &routing,
                "selected",
                &reordered_envelope,
            )
            .unwrap(),
            "fallback order is authorization-significant"
        );
        let invalid_envelope = vec!["fallback-a".to_string(), "selected".to_string()];
        assert!(
            item_authorization_hash(
                "/repos/alpha",
                &issue,
                &routing,
                "selected",
                &invalid_envelope,
            )
            .is_err(),
            "selected model must remain the first approved identity"
        );

        let mut unrelated = issue.clone();
        unrelated.status = "in_progress".to_string();
        unrelated.priority = 0;
        unrelated.assignee = Some("someone".to_string());
        unrelated.updated_at = "2099-01-01T00:00:00Z".to_string();
        unrelated.metadata = Some(BTreeMap::from([(
            "unrelated".to_string(),
            serde_json::json!(true),
        )]));
        assert_eq!(
            baseline,
            item_authorization_hash("/repos/alpha", &unrelated, &routing, "selected", &envelope,)
                .unwrap()
        );
    }

    #[test]
    fn legacy_plan_without_scope_or_hashes_fails_closed() {
        let legacy = serde_json::json!({
            "cycle_id": "cycle",
            "created_at": "2026-07-13T00:00:00Z",
            "dispatches": [],
            "proposals": [],
            "flags": [],
            "skips": [],
            "provider_routes": []
        });
        assert!(serde_json::from_value::<CyclePlan>(legacy).is_err());
    }

    #[test]
    fn cycle_plan_load_rejects_noncanonical_persisted_scope() {
        let tmp = std::env::temp_dir().join("conductor-invalid-scope-plan-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("plans")).unwrap();
        let invalid = serde_json::json!({
            "cycle_id": "cycle",
            "created_at": "2026-07-13T00:00:00Z",
            "dispatches": [],
            "proposals": [],
            "flags": [],
            "skips": [],
            "provider_routes": [],
            "approval_scope": {
                "kind": "repository-scope",
                "selectors": [
                    {"kind": "repository", "repo": "/repos/bravo"},
                    {"kind": "repository", "repo": "/repos/alpha"}
                ],
                "repo_paths": ["/repos/bravo", "/repos/alpha"],
                "max_dispatch_count": 2
            },
            "item_authorizations": []
        });
        std::fs::write(
            tmp.join("plans/cycle.json"),
            serde_json::to_vec_pretty(&invalid).unwrap(),
        )
        .unwrap();
        assert!(CyclePlan::load(&tmp, "cycle").is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn authorization_issue() -> Issue {
        Issue {
            id: "alpha-1".to_string(),
            title: "Implement bounded approval".to_string(),
            description: "Persist the exact scope".to_string(),
            acceptance_criteria: "No approval widening".to_string(),
            notes: "tier_floor: senior".to_string(),
            status: "open".to_string(),
            priority: 1,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "owner".to_string(),
            created_at: "2026-07-13T00:00:00Z".to_string(),
            created_by: "owner".to_string(),
            updated_at: "2026-07-13T00:00:00Z".to_string(),
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
}
