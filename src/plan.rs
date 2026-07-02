//! cycle plan build/serialize (~/.local/state/conductor/plans/<cycle-id>.json)

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::triage::{Flag, Plan, SkipCode};

/// Serializable cycle plan written to the state dir.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CyclePlan {
    pub(crate) cycle_id: String,
    pub(crate) created_at: String,
    pub(crate) dispatches: Vec<DispatchEntry>,
    pub(crate) proposals: Vec<ProposalEntry>,
    pub(crate) flags: Vec<FlagEntry>,
    pub(crate) skips: Vec<SkipEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DispatchEntry {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
    pub(crate) verify_cmd: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProposalEntry {
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FlagEntry {
    pub(crate) kind: String,
    pub(crate) repo: String,
    pub(crate) issue_id: String,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, Serialize)]
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
        }
    }

    /// Writes the plan JSON to `<state_dir>/plans/<cycle-id>.json`.
    pub(crate) fn save(&self, state_dir: &Path) -> io::Result<PathBuf> {
        let plans_dir = state_dir.join("plans");
        std::fs::create_dir_all(&plans_dir)?;
        let path = plans_dir.join(format!("{}.json", self.cycle_id));
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(&path, json)?;
        Ok(path)
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
}
