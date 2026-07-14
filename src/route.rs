#[cfg(test)]
mod tests {
    use super::super::bursar::{Availability, BudgetAction, BudgetDecision};
    use super::super::config::{Backend, Ceiling, Cost, CostPolicy, Efficiency, RosterEntry, Tier};
    use super::super::fields::RoutingFields;
    use super::super::triage::CandidateRejection;
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn roster_entry(
        name: &str,
        tier: Tier,
        ceiling: Ceiling,
        efficiency: Efficiency,
        provider: &str,
        cost: Cost,
    ) -> RosterEntry {
        RosterEntry {
            name: name.to_string(),
            tier,
            ceiling,
            efficiency,
            backend: Backend::Pi,
            dispatch_id: format!("dispatch-{name}"),
            reasoning_effort: None,
            provider: provider.to_string(),
            cost,
            fallback: Vec::new(),
        }
    }

    fn paid_entry(name: &str, tier: Tier, efficiency: Efficiency, provider: &str) -> RosterEntry {
        roster_entry(name, tier, Ceiling::M, efficiency, provider, Cost::Paid)
    }

    fn decision(
        provider: &str,
        action: BudgetAction,
        availability: Availability,
    ) -> BudgetDecision {
        BudgetDecision {
            provider: provider.to_string(),
            model: Some(format!("{provider}-model")),
            availability: Some(availability),
            source: Some("fixture".to_string()),
            checked_at: Some("2026-07-13T12:00:00Z".to_string()),
            data_as_of: Some("2026-07-13T11:59:00Z".to_string()),
            expires_at: Some("2026-07-13T12:05:00Z".to_string()),
            expiry_basis: Some("provider-reset".to_string()),
            action,
            summary: format!("{provider}: {}", action.label()),
        }
    }

    fn decisions(
        entries: &[(&str, BudgetAction, Availability)],
    ) -> BTreeMap<String, BudgetDecision> {
        entries
            .iter()
            .map(|(provider, action, availability)| {
                (
                    (*provider).to_string(),
                    decision(provider, *action, *availability),
                )
            })
            .collect()
    }

    #[test]
    fn provider_ordering_runs_after_hard_gates_and_returns_full_audit() {
        let roster = vec![
            paid_entry("caution", Tier::Senior, Efficiency::Lean, "opencode-go"),
            paid_entry("healthy", Tier::Senior, Efficiency::Std, "codex"),
            paid_entry("unknown", Tier::Senior, Efficiency::Lean, "agy"),
            paid_entry("too-low", Tier::Junior, Efficiency::Lean, "anthropic"),
            roster_entry(
                "data-blocked",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                "google-ai-studio",
                Cost::FreeTrainsInput,
            ),
            paid_entry("healthy-alt", Tier::Senior, Efficiency::Heavy, "neuralwatt"),
        ];
        let routing = RoutingFields {
            tier_floor: Tier::Senior,
            complexity: Ceiling::M,
            verify_cmd: Some("cargo test".to_string()),
            trains_ok: false,
        };
        let advice = select(
            "repo",
            &routing,
            CostPolicy::Proprietary,
            &roster,
            &decisions(&[
                (
                    "opencode-go",
                    BudgetAction::SpendCautiously,
                    Availability::Caution,
                ),
                ("codex", BudgetAction::Proceed, Availability::Healthy),
                ("agy", BudgetAction::Defer, Availability::Unknown),
                ("anthropic", BudgetAction::Proceed, Availability::Healthy),
                (
                    "google-ai-studio",
                    BudgetAction::Proceed,
                    Availability::Healthy,
                ),
                ("neuralwatt", BudgetAction::Proceed, Availability::Healthy),
            ]),
            &HashMap::new(),
            None,
        );

        assert_eq!(
            advice
                .selected
                .as_ref()
                .map(|candidate| candidate.model.as_str()),
            Some("healthy")
        );
        assert_eq!(advice.alternatives.len(), 2);
        assert!(advice.audit.iter().any(|entry| {
            entry.model == "unknown"
                && entry
                    .reasons
                    .iter()
                    .any(|reason| reason.code == "provider-unknown")
        }));
        assert!(advice.audit.iter().any(|entry| {
            entry.model == "too-low"
                && entry.reasons.iter().any(|reason| {
                    matches!(
                        reason.rejection,
                        Some(CandidateRejection::BelowTierFloor { .. })
                    )
                })
        }));
        assert!(advice.audit.iter().any(|entry| {
            entry.model == "data-blocked"
                && entry.reasons.iter().any(|reason| {
                    matches!(
                        reason.rejection,
                        Some(CandidateRejection::CostPolicy { .. })
                    )
                })
        }));
        assert!(advice
            .audit
            .iter()
            .all(|entry| entry.reasons.iter().all(|reason| !reason.text.is_empty())));
    }

    #[test]
    fn configured_fallback_is_selected_only_when_it_remains_eligible() {
        let mut primary = paid_entry("primary", Tier::Senior, Efficiency::Lean, "opencode-go");
        primary.fallback = vec!["fallback".to_string(), "too-expensive".to_string()];
        let roster = vec![
            primary,
            paid_entry("fallback", Tier::Senior, Efficiency::Std, "codex"),
            paid_entry("unconfigured", Tier::Senior, Efficiency::Lean, "anthropic"),
            roster_entry(
                "too-expensive",
                Tier::Senior,
                Ceiling::M,
                Efficiency::Lean,
                "agy",
                Cost::FreeTrainsInput,
            ),
        ];
        let routing = RoutingFields {
            tier_floor: Tier::Senior,
            complexity: Ceiling::M,
            verify_cmd: None,
            trains_ok: false,
        };
        let advice = select(
            "repo",
            &routing,
            CostPolicy::Proprietary,
            &roster,
            &decisions(&[
                ("opencode-go", BudgetAction::Defer, Availability::Exhausted),
                ("codex", BudgetAction::Proceed, Availability::Healthy),
                ("anthropic", BudgetAction::Proceed, Availability::Healthy),
                ("agy", BudgetAction::Proceed, Availability::Healthy),
            ]),
            &HashMap::new(),
            None,
        );

        assert_eq!(
            advice
                .selected
                .as_ref()
                .map(|candidate| candidate.model.as_str()),
            Some("fallback")
        );
        assert!(advice
            .selected
            .as_ref()
            .is_some_and(|candidate| { candidate.fallback.is_empty() }));
        assert!(advice
            .audit
            .iter()
            .any(|entry| { entry.model == "too-expensive" && !entry.eligible }));
        assert!(advice.audit.iter().any(|entry| {
            entry.model == "unconfigured"
                && !entry.eligible
                && entry
                    .reasons
                    .iter()
                    .any(|reason| reason.code == "outside-fallback-chain")
        }));
    }

    #[test]
    fn disabled_bursar_keeps_providerless_legacy_rows_on_static_caps() {
        let roster = vec![paid_entry("legacy", Tier::Senior, Efficiency::Lean, "")];
        let decisions = snapshot_provider_decisions(
            &crate::bursar::test_support::FakeBursarClient::unavailable(),
            &roster,
            false,
        );
        let advice = select(
            "repo",
            &RoutingFields {
                tier_floor: Tier::Senior,
                complexity: Ceiling::M,
                verify_cmd: None,
                trains_ok: false,
            },
            CostPolicy::Proprietary,
            &roster,
            &decisions,
            &HashMap::new(),
            None,
        );

        assert_eq!(
            advice
                .selected
                .as_ref()
                .map(|candidate| candidate.model.as_str()),
            Some("legacy")
        );
        assert_eq!(
            advice
                .selected
                .and_then(|candidate| candidate.evidence)
                .map(|evidence| evidence.action),
            Some(BudgetAction::StaticCaps)
        );
    }

    #[test]
    fn provider_state_does_not_promote_a_higher_tier_over_an_available_lower_tier() {
        let roster = vec![
            paid_entry(
                "senior-caution",
                Tier::Senior,
                Efficiency::Lean,
                "opencode-go",
            ),
            paid_entry("lead-healthy", Tier::Lead, Efficiency::Lean, "anthropic"),
        ];
        let advice = select(
            "repo",
            &RoutingFields {
                tier_floor: Tier::Senior,
                complexity: Ceiling::M,
                verify_cmd: None,
                trains_ok: false,
            },
            CostPolicy::Proprietary,
            &roster,
            &decisions(&[
                (
                    "opencode-go",
                    BudgetAction::SpendCautiously,
                    Availability::Caution,
                ),
                ("anthropic", BudgetAction::Proceed, Availability::Healthy),
            ]),
            &HashMap::new(),
            None,
        );

        assert_eq!(
            advice
                .selected
                .as_ref()
                .map(|candidate| candidate.model.as_str()),
            Some("senior-caution")
        );
    }
}
use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

use serde_json::{json, Value};

use crate::bursar::{
    evaluate_budget, Availability, BudgetAction, BudgetDecision, BursarClient, StatusReport,
};
use crate::config::{Backend, Config, Cost, CostPolicy, Efficiency, RosterEntry, Tier};
use crate::fields::RoutingFields;
use crate::triage::{candidate_rejection, CandidateRejection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteIntent {
    CheapWork,
    OutsidePerspective,
}

impl FromStr for RouteIntent {
    type Err = RouteError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cheap-work" => Ok(Self::CheapWork),
            "outside-perspective" => Ok(Self::OutsidePerspective),
            _ => Err(RouteError::new(
                "--intent must be cheap-work or outside-perspective",
            )),
        }
    }
}

impl RouteIntent {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::CheapWork => "cheap-work",
            Self::OutsidePerspective => "outside-perspective",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteError {
    message: String,
}

impl RouteError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RouteError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderEvidence {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) availability: Option<Availability>,
    pub(crate) source: Option<String>,
    pub(crate) checked_at: Option<String>,
    pub(crate) data_as_of: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) expiry_basis: Option<String>,
    pub(crate) action: BudgetAction,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteCandidate {
    pub(crate) model: String,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) provider: String,
    pub(crate) cost: Cost,
    pub(crate) fallback: Vec<String>,
    pub(crate) evidence: Option<ProviderEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteReason {
    pub(crate) code: String,
    pub(crate) text: String,
    pub(crate) rejection: Option<CandidateRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateAudit {
    pub(crate) model: String,
    pub(crate) eligible: bool,
    pub(crate) candidate: RouteCandidate,
    pub(crate) reasons: Vec<RouteReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteAdvice {
    pub(crate) repo: String,
    pub(crate) dispatch_excluded: bool,
    pub(crate) intent: Option<RouteIntent>,
    pub(crate) selected: Option<RouteCandidate>,
    pub(crate) alternatives: Vec<RouteCandidate>,
    pub(crate) audit: Vec<CandidateAudit>,
}

struct FrozenBursar {
    result: crate::bursar::Result<StatusReport>,
}

impl BursarClient for FrozenBursar {
    fn status(&self) -> crate::bursar::Result<StatusReport> {
        self.result.clone()
    }
}

pub(crate) fn snapshot_provider_decisions(
    client: &dyn BursarClient,
    roster: &[RosterEntry],
    use_bursar: bool,
) -> BTreeMap<String, BudgetDecision> {
    let snapshot = use_bursar.then(|| client.status());
    let frozen = FrozenBursar {
        result: snapshot
            .unwrap_or_else(|| Err(crate::bursar::BursarError::unavailable("not queried"))),
    };
    roster
        .iter()
        .map(|entry| entry.provider.as_str())
        .map(crate::bursar::canonical_provider_key)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|provider| {
            (
                provider.to_string(),
                evaluate_budget(&frozen, provider, use_bursar),
            )
        })
        .collect()
}

pub(crate) fn explain(
    config: &Config,
    repo_path: &Path,
    routing: &RoutingFields,
    intent: Option<RouteIntent>,
    client: &dyn BursarClient,
) -> RouteAdvice {
    let repo = repo_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| repo_path.to_str().unwrap_or_default());
    let decisions = snapshot_provider_decisions(client, &config.roster, config.budgets.use_bursar);
    select(
        repo,
        routing,
        config.cost_policy_for(repo),
        &config.roster,
        &decisions,
        &HashMap::new(),
        intent,
    )
}

pub(crate) fn select(
    repo: &str,
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
    roster: &[RosterEntry],
    provider_decisions: &BTreeMap<String, BudgetDecision>,
    dispatch_count_by_model: &HashMap<String, u32>,
    intent: Option<RouteIntent>,
) -> RouteAdvice {
    select_impl(
        repo,
        routing,
        repo_cost_policy,
        roster,
        Some(provider_decisions),
        dispatch_count_by_model,
        intent,
    )
}

pub(crate) fn select_legacy<'a>(
    roster: &'a [RosterEntry],
    routing: &RoutingFields,
    repo: &str,
    repo_cost_policy: CostPolicy,
    dispatch_count_by_model: &HashMap<String, u32>,
) -> Option<&'a RosterEntry> {
    select_impl(
        repo,
        routing,
        repo_cost_policy,
        roster,
        None,
        dispatch_count_by_model,
        None,
    )
    .selected
    .and_then(|selected| roster.iter().find(|entry| entry.name == selected.model))
}

fn select_impl(
    repo: &str,
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
    roster: &[RosterEntry],
    provider_decisions: Option<&BTreeMap<String, BudgetDecision>>,
    dispatch_count_by_model: &HashMap<String, u32>,
    intent: Option<RouteIntent>,
) -> RouteAdvice {
    let (mut audit, eligible, fallback_names) = evaluate_candidates(
        routing,
        repo_cost_policy,
        roster,
        provider_decisions,
        dispatch_count_by_model,
    );
    let mut ranked = eligible;
    ranked.sort_by_key(|(index, entry, decision)| {
        let provider_rank = decision.map_or(0, |d| provider_rank(d.action));
        let fallback_rank = fallback_names.map_or(usize::MAX, |names| {
            names
                .iter()
                .position(|name| name == &entry.name)
                .unwrap_or(usize::MAX)
        });
        (
            tier_rank(entry.tier),
            provider_rank,
            fallback_rank,
            intent_rank(entry, intent),
            efficiency_rank(entry.efficiency),
            *dispatch_count_by_model.get(&entry.name).unwrap_or(&0),
            *index,
        )
    });

    let selected = ranked
        .first()
        .map(|(_, entry, decision)| candidate(entry, *decision));
    let alternatives = ranked
        .iter()
        .skip(1)
        .take(2)
        .map(|(_, entry, decision)| candidate(entry, *decision))
        .collect::<Vec<_>>();
    if let Some(selected) = selected.as_ref() {
        if let Some(entry) = audit.iter_mut().find(|item| item.model == selected.model) {
            entry.reasons.push(RouteReason {
                code: "selected-current-algorithm".to_string(),
                text: "selected by provider-aware tier and roster ordering".to_string(),
                rejection: None,
            });
        }
    }
    for entry in &mut audit {
        if entry.eligible && entry.reasons.is_empty() {
            entry.reasons.push(RouteReason {
                code: "eligible-alternative".to_string(),
                text: "eligible but ranked behind the selected route".to_string(),
                rejection: None,
            });
        }
    }

    RouteAdvice {
        repo: repo.to_string(),
        dispatch_excluded: crate::config::HARDCODED_EXCLUDE.contains(&repo),
        intent,
        selected,
        alternatives,
        audit,
    }
}

type RankedCandidate<'r, 'd> = (usize, &'r RosterEntry, Option<&'d BudgetDecision>);

fn evaluate_candidates<'r, 'd>(
    routing: &RoutingFields,
    repo_cost_policy: CostPolicy,
    roster: &'r [RosterEntry],
    provider_decisions: Option<&'d BTreeMap<String, BudgetDecision>>,
    dispatch_count_by_model: &HashMap<String, u32>,
) -> (
    Vec<CandidateAudit>,
    Vec<RankedCandidate<'r, 'd>>,
    Option<&'r [String]>,
) {
    let legacy_first = roster
        .iter()
        .enumerate()
        .filter(|(_, entry)| candidate_rejection(entry, routing, repo_cost_policy).is_none())
        .min_by_key(|(index, entry)| base_key(entry, *index, dispatch_count_by_model));
    let fallback_names = provider_decisions.and_then(|decisions| {
        legacy_first.and_then(|(_, entry)| {
            let decision = decisions.get(crate::bursar::canonical_provider_key(&entry.provider));
            decision
                .is_none_or(|decision| !provider_is_eligible(decision))
                .then_some(entry.fallback.as_slice())
        })
    });
    let mut audit = Vec::with_capacity(roster.len());
    let mut eligible = Vec::new();

    for (index, entry) in roster.iter().enumerate() {
        let decision = provider_decisions.and_then(|decisions| {
            decisions.get(crate::bursar::canonical_provider_key(&entry.provider))
        });
        let mut reasons = Vec::new();
        if let Some(rejection) = candidate_rejection(entry, routing, repo_cost_policy) {
            reasons.push(hard_gate_reason(rejection));
        } else if let Some(decision) = decision {
            if !provider_is_eligible(decision) {
                reasons.push(provider_reason(decision));
            } else if fallback_names
                .is_some_and(|names| !names.iter().any(|name| name == &entry.name))
            {
                reasons.push(RouteReason {
                    code: "outside-fallback-chain".to_string(),
                    text: "provider is available but the model is outside the configured fallback chain"
                        .to_string(),
                    rejection: None,
                });
            } else {
                eligible.push((index, entry, Some(decision)));
            }
        } else if provider_decisions.is_some() {
            reasons.push(RouteReason {
                code: "provider-unknown".to_string(),
                text: format!("{}: no trusted Bursar decision", entry.provider),
                rejection: None,
            });
        } else {
            eligible.push((index, entry, None));
        }
        audit.push(CandidateAudit {
            model: entry.name.clone(),
            eligible: reasons.is_empty(),
            candidate: candidate(entry, decision),
            reasons,
        });
    }

    (audit, eligible, fallback_names)
}

fn candidate(entry: &RosterEntry, decision: Option<&BudgetDecision>) -> RouteCandidate {
    RouteCandidate {
        model: entry.name.clone(),
        backend: entry.backend,
        dispatch_id: entry.dispatch_id.clone(),
        reasoning_effort: entry
            .reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        provider: entry.provider.clone(),
        cost: entry.cost,
        fallback: entry.fallback.clone(),
        evidence: decision.map(provider_evidence),
    }
}

fn provider_evidence(decision: &BudgetDecision) -> ProviderEvidence {
    ProviderEvidence {
        provider: decision.provider.clone(),
        model: decision.model.clone(),
        availability: decision.availability,
        source: decision.source.clone(),
        checked_at: decision.checked_at.clone(),
        data_as_of: decision.data_as_of.clone(),
        expires_at: decision.expires_at.clone(),
        expiry_basis: decision.expiry_basis.clone(),
        action: decision.action,
        reason: decision.summary.clone(),
    }
}

fn provider_is_eligible(decision: &BudgetDecision) -> bool {
    matches!(
        decision.action,
        BudgetAction::Proceed | BudgetAction::SpendCautiously | BudgetAction::StaticCaps
    )
}

fn provider_rank(action: BudgetAction) -> u8 {
    match action {
        BudgetAction::Proceed | BudgetAction::StaticCaps => 0,
        BudgetAction::SpendCautiously => 1,
        BudgetAction::Defer => 2,
    }
}

fn provider_reason(decision: &BudgetDecision) -> RouteReason {
    let code = match decision.availability {
        Some(Availability::Exhausted) => "provider-exhausted",
        Some(Availability::Unknown) | None => "provider-unknown",
        Some(Availability::Healthy | Availability::Caution) => "provider-deferred",
    };
    RouteReason {
        code: code.to_string(),
        text: decision.summary.clone(),
        rejection: None,
    }
}

fn hard_gate_reason(rejection: CandidateRejection) -> RouteReason {
    let (code, text) = match rejection {
        CandidateRejection::BelowTierFloor { required, actual } => (
            "tier-below-floor",
            format!("tier {actual:?} is below required floor {required:?}"),
        ),
        CandidateRejection::BelowCeiling { required, actual } => (
            "ceiling-too-low",
            format!("ceiling {actual:?} is below required complexity {required:?}"),
        ),
        CandidateRejection::CostPolicy { policy, cost } => (
            "repo-cost-policy",
            format!("cost {cost:?} is not allowed by repo policy {policy:?}"),
        ),
    };
    RouteReason {
        code: code.to_string(),
        text,
        rejection: Some(rejection),
    }
}

fn base_key(
    entry: &RosterEntry,
    index: usize,
    dispatch_count_by_model: &HashMap<String, u32>,
) -> (u8, u8, u32, usize) {
    (
        tier_rank(entry.tier),
        efficiency_rank(entry.efficiency),
        *dispatch_count_by_model.get(&entry.name).unwrap_or(&0),
        index,
    )
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

fn intent_rank(entry: &RosterEntry, intent: Option<RouteIntent>) -> u8 {
    match intent {
        Some(RouteIntent::CheapWork) => match entry.cost {
            Cost::Free => 0,
            Cost::FreeTrainsInput => 1,
            Cost::Paid => 2,
        },
        Some(RouteIntent::OutsidePerspective) => u8::from(matches!(entry.backend, Backend::Claude)),
        None => 0,
    }
}

impl RouteAdvice {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "repo": self.repo,
            "dispatch_excluded": self.dispatch_excluded,
            "intent": self.intent.map(RouteIntent::label),
            "selected": self.selected.as_ref().map(candidate_json),
            "alternatives": self.alternatives.iter().map(candidate_json).collect::<Vec<_>>(),
            "audit": self.audit.iter().map(audit_json).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn human(&self) -> String {
        let mut output = format!(
            "repo: {}\ndispatch excluded: {}\n",
            self.repo, self.dispatch_excluded
        );
        match self.selected.as_ref() {
            Some(candidate) => write_candidate_human(&mut output, "selected:", candidate),
            None => output.push_str("selected: none\n"),
        }
        if self.alternatives.is_empty() {
            output.push_str("alternatives: none\n");
        } else {
            output.push_str("alternatives:\n");
            for candidate in &self.alternatives {
                write_candidate_human(&mut output, "-", candidate);
            }
        }
        output.push_str("\nCANDIDATE AUDIT\n");
        for entry in &self.audit {
            let reasons = entry
                .reasons
                .iter()
                .map(|reason| format!("{} ({})", reason.code, reason.text))
                .collect::<Vec<_>>()
                .join("; ");
            writeln!(
                &mut output,
                "- {}: eligible={} — {}",
                entry.model, entry.eligible, reasons
            )
            .expect("writing route advice to a String cannot fail");
        }
        output
    }
}

fn write_candidate_human(output: &mut String, label: &str, candidate: &RouteCandidate) {
    let evidence = candidate.evidence.as_ref();
    writeln!(
        output,
        "{label} {} backend={} dispatch_id={} reasoning_effort={} provider={} availability={} source={} checked_at={} data_as_of={} expires_at={} action={} reason={}",
        candidate.model,
        backend_label(candidate.backend),
        candidate.dispatch_id,
        candidate.reasoning_effort.as_deref().unwrap_or("none"),
        candidate.provider,
        evidence
            .and_then(|value| value.availability)
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
        evidence.and_then(|value| value.source.as_deref()).unwrap_or("none"),
        evidence
            .and_then(|value| value.checked_at.as_deref())
            .unwrap_or("none"),
        evidence
            .and_then(|value| value.data_as_of.as_deref())
            .unwrap_or("none"),
        evidence
            .and_then(|value| value.expires_at.as_deref())
            .unwrap_or("none"),
        evidence.map_or("none", |value| value.action.label()),
        evidence.map_or("none", |value| value.reason.as_str()),
    )
    .expect("writing route candidate to a String cannot fail");
}

const fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "claude",
        Backend::Pi => "pi",
        Backend::Agy => "agy",
        Backend::Codex => "codex",
    }
}

fn candidate_json(candidate: &RouteCandidate) -> Value {
    json!({
        "model": candidate.model,
        "backend": backend_label(candidate.backend),
        "dispatch_id": candidate.dispatch_id,
        "reasoning_effort": candidate.reasoning_effort,
        "provider": candidate.provider,
        "cost": format!("{:?}", candidate.cost).to_ascii_lowercase(),
        "fallback": candidate.fallback,
        "provider_evidence": candidate.evidence.as_ref().map(evidence_json),
    })
}

fn evidence_json(evidence: &ProviderEvidence) -> Value {
    json!({
        "provider": evidence.provider,
        "model": evidence.model,
        "availability": evidence.availability.map(|value| value.to_string()),
        "source": evidence.source,
        "checked_at": evidence.checked_at,
        "data_as_of": evidence.data_as_of,
        "expires_at": evidence.expires_at,
        "expiry_basis": evidence.expiry_basis,
        "action": evidence.action.label(),
        "reason": evidence.reason,
    })
}

fn audit_json(audit: &CandidateAudit) -> Value {
    json!({
        "model": audit.model,
        "eligible": audit.eligible,
        "candidate": candidate_json(&audit.candidate),
        "reasons": audit.reasons.iter().map(|reason| json!({
            "code": reason.code,
            "text": reason.text,
        })).collect::<Vec<_>>(),
    })
}
