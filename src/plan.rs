//! cycle plan build/serialize (~/.local/state/conductor/plans/<cycle-id>.json)

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

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
        serde_json::from_slice(&bytes).map_err(io::Error::other)
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
}
