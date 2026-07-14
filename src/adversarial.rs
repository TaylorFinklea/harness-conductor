//! Approval-gated, read-only adversarial design review state.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::BTreeMap, collections::HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bursar::{self, Availability, BudgetAction, BudgetDecision};
use crate::config::{AdversarialReviewConfig, Cost, Efficiency, RosterEntry, Tier};

const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const MAX_REVIEW_ID_BYTES: usize = 128;
const ARTIFACT_FILE: &str = "artifact.bin";

#[derive(Debug)]
pub(crate) struct AdversarialError(String);

impl AdversarialError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn io(action: &str, path: &Path, error: &std::io::Error) -> Self {
        Self(format!("{action} {}: {error}", path.display()))
    }
}

impl fmt::Display for AdversarialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AdversarialError {}

type Result<T> = std::result::Result<T, AdversarialError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtifactSnapshot {
    pub(crate) source_path: PathBuf,
    pub(crate) snapshot_path: PathBuf,
    pub(crate) review_dir: PathBuf,
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewerSlot {
    pub(crate) slot: usize,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) alternatives: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JudgeSlot {
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PanelCandidateAudit {
    pub(crate) role: String,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) availability: String,
    pub(crate) outcome: String,
    pub(crate) reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PanelPlan {
    pub(crate) reviewers: Vec<ReviewerSlot>,
    pub(crate) judge: JudgeSlot,
    pub(crate) audit: Vec<PanelCandidateAudit>,
}

#[derive(Clone, Copy)]
struct EligibleCandidate<'a> {
    entry: &'a RosterEntry,
    roster_index: usize,
    action: BudgetAction,
}

pub(crate) fn plan_panel(
    roster: &[RosterEntry],
    config: &AdversarialReviewConfig,
    provider_snapshot: &BTreeMap<String, BudgetDecision>,
    reviewer_count: usize,
    explicit_models: Option<&[String]>,
) -> Result<PanelPlan> {
    if reviewer_count == 0 || reviewer_count > config.max_reviewers as usize {
        return Err(AdversarialError::new(format!(
            "reviewer count must be between 1 and {}",
            config.max_reviewers
        )));
    }
    if let Some(models) = explicit_models
        && models.len() != reviewer_count
    {
        return Err(AdversarialError::new(format!(
            "explicit model list contains {} entries; expected {reviewer_count}",
            models.len()
        )));
    }

    let mut audit = Vec::with_capacity(roster.len() + config.judge_fallbacks.len() + 1);
    let mut eligible = Vec::new();
    for (roster_index, entry) in roster.iter().enumerate() {
        let provider = bursar::normalize_provider_key(&entry.provider);
        let (decision, reasons) = reviewer_eligibility(entry, &provider, provider_snapshot);
        let availability = decision.map_or_else(|| "unknown".to_string(), decision_availability);
        if reasons.is_empty() {
            eligible.push(EligibleCandidate {
                entry,
                roster_index,
                action: decision
                    .expect("eligible reviewer has provider decision")
                    .action,
            });
        }
        audit.push(PanelCandidateAudit {
            role: "reviewer".to_string(),
            model: entry.name.clone(),
            provider,
            availability,
            outcome: if reasons.is_empty() {
                "eligible".to_string()
            } else {
                "excluded".to_string()
            },
            reasons,
        });
    }

    let selected = if let Some(models) = explicit_models {
        select_explicit_reviewers(roster, &eligible, models, reviewer_count)?
    } else {
        select_automatic_reviewers(&eligible, reviewer_count)?
    };
    let reviewers = build_reviewer_slots(&eligible, &selected);
    let selection_reason = if explicit_models.is_some() {
        "explicit model passed closed-roster, tier, data, health, and provider-distinctness gates"
    } else {
        "selected by provider health, then cost, tier, efficiency, and roster order"
    };
    for slot in &reviewers {
        mark_audit_outcome(&mut audit, "reviewer", &slot.model, "selected-reviewer");
        add_audit_reason(&mut audit, "reviewer", &slot.model, selection_reason);
        for alternative in &slot.alternatives {
            mark_audit_outcome(
                &mut audit,
                "reviewer",
                alternative,
                "approved-same-provider-alternative",
            );
            add_audit_reason(
                &mut audit,
                "reviewer",
                alternative,
                "same-provider alternative ordered by cost, tier, efficiency, and roster order",
            );
        }
    }
    for row in &mut audit {
        if row.role == "reviewer" && row.outcome == "eligible" {
            row.outcome = "eligible-not-selected".to_string();
        }
    }

    let (judge, mut judge_audit) = select_judge(roster, config, provider_snapshot, &reviewers)?;
    audit.append(&mut judge_audit);
    Ok(PanelPlan {
        reviewers,
        judge,
        audit,
    })
}

fn reviewer_eligibility<'a>(
    entry: &RosterEntry,
    provider: &str,
    provider_snapshot: &'a BTreeMap<String, BudgetDecision>,
) -> (Option<&'a BudgetDecision>, Vec<String>) {
    let mut reasons = Vec::new();
    if !matches!(entry.tier, Tier::Senior | Tier::Lead) {
        reasons.push("reviewers must be Senior or Lead".to_string());
    }
    if provider.is_empty() {
        reasons.push("provider key is empty".to_string());
    }
    if entry.cost == Cost::FreeTrainsInput {
        reasons.push(
            "free-trains-input is not allowed for proprietary adversarial artifacts".to_string(),
        );
    }
    let decision = provider_snapshot.get(provider);
    match decision {
        None => reasons.push("provider is absent from the trusted snapshot".to_string()),
        Some(decision) if !decision_is_eligible(decision) => reasons.push(format!(
            "provider is not eligible: {}",
            decision.availability.map_or_else(
                || decision.action.label().to_string(),
                |value| value.to_string()
            )
        )),
        Some(_) => {}
    }
    (decision, reasons)
}

fn select_explicit_reviewers<'a>(
    roster: &'a [RosterEntry],
    eligible: &[EligibleCandidate<'a>],
    models: &[String],
    reviewer_count: usize,
) -> Result<Vec<EligibleCandidate<'a>>> {
    if models.len() != reviewer_count {
        return Err(AdversarialError::new("explicit reviewer count mismatch"));
    }
    let mut names = HashSet::new();
    let mut providers = HashSet::new();
    let mut selected: Vec<EligibleCandidate<'a>> = Vec::with_capacity(models.len());
    for model in models {
        if !names.insert(model.as_str()) {
            return Err(AdversarialError::new(format!(
                "explicit reviewer model is duplicated: {model}"
            )));
        }
        let entry = roster
            .iter()
            .find(|entry| entry.name == *model)
            .ok_or_else(|| {
                AdversarialError::new(format!(
                    "explicit reviewer is not in the closed roster: {model}"
                ))
            })?;
        let candidate = eligible
            .iter()
            .find(|candidate| candidate.entry.name == entry.name)
            .copied()
            .ok_or_else(|| {
                AdversarialError::new(format!(
                    "explicit reviewer does not satisfy tier, data, or provider gates: {model}"
                ))
            })?;
        let provider = bursar::normalize_provider_key(&candidate.entry.provider);
        if !providers.insert(provider.clone()) {
            return Err(AdversarialError::new(format!(
                "explicit reviewers do not use distinct providers: {provider}"
            )));
        }
        if selected
            .iter()
            .any(|other| same_dispatch_identity(other.entry, candidate.entry))
        {
            return Err(AdversarialError::new(format!(
                "explicit reviewer aliases an already selected dispatch identity: {model}"
            )));
        }
        selected.push(candidate);
    }
    Ok(selected)
}

fn select_automatic_reviewers<'a>(
    eligible: &[EligibleCandidate<'a>],
    reviewer_count: usize,
) -> Result<Vec<EligibleCandidate<'a>>> {
    let mut groups: BTreeMap<String, Vec<EligibleCandidate<'a>>> = BTreeMap::new();
    for candidate in eligible {
        groups
            .entry(bursar::normalize_provider_key(&candidate.entry.provider))
            .or_default()
            .push(*candidate);
    }
    for candidates in groups.values_mut() {
        candidates.sort_by_key(candidate_key);
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by_key(|(provider, candidates)| {
        (
            health_rank(candidates[0].action),
            candidate_key(&candidates[0]),
            provider.clone(),
        )
    });
    if groups.len() < reviewer_count {
        return Err(AdversarialError::new(format!(
            "provider shortfall: requested {reviewer_count} distinct reviewers but only {} eligible provider groups remain ({})",
            groups.len(),
            groups
                .iter()
                .map(|(provider, _)| provider.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(groups
        .into_iter()
        .take(reviewer_count)
        .map(|(_, candidates)| candidates[0])
        .collect())
}

fn build_reviewer_slots<'a>(
    eligible: &[EligibleCandidate<'a>],
    selected: &[EligibleCandidate<'a>],
) -> Vec<ReviewerSlot> {
    selected
        .iter()
        .enumerate()
        .map(|(index, primary)| {
            let provider = bursar::normalize_provider_key(&primary.entry.provider);
            let mut alternatives = eligible
                .iter()
                .filter(|candidate| {
                    bursar::normalize_provider_key(&candidate.entry.provider) == provider
                        && candidate.entry.name != primary.entry.name
                        && !same_dispatch_identity(candidate.entry, primary.entry)
                })
                .copied()
                .collect::<Vec<_>>();
            alternatives.sort_by_key(candidate_key);
            ReviewerSlot {
                slot: index + 1,
                model: primary.entry.name.clone(),
                provider,
                alternatives: alternatives
                    .into_iter()
                    .take(2)
                    .map(|candidate| candidate.entry.name.clone())
                    .collect(),
            }
        })
        .collect()
}

fn select_judge(
    roster: &[RosterEntry],
    config: &AdversarialReviewConfig,
    provider_snapshot: &BTreeMap<String, BudgetDecision>,
    reviewers: &[ReviewerSlot],
) -> Result<(JudgeSlot, Vec<PanelCandidateAudit>)> {
    let reviewer_chain = reviewers
        .iter()
        .flat_map(|slot| std::iter::once(&slot.model).chain(slot.alternatives.iter()))
        .filter_map(|name| roster.iter().find(|entry| entry.name == *name))
        .collect::<Vec<_>>();
    let chain = std::iter::once(&config.judge)
        .chain(config.judge_fallbacks.iter())
        .collect::<Vec<_>>();
    let mut eligible = Vec::new();
    let mut audit = Vec::with_capacity(chain.len());
    for name in chain {
        let mut reasons = Vec::new();
        let entry = roster.iter().find(|entry| entry.name == *name);
        let provider = entry.map_or_else(String::new, |entry| {
            bursar::normalize_provider_key(&entry.provider)
        });
        let decision = provider_snapshot.get(&provider);
        match entry {
            None => reasons.push("judge is not in the closed roster".to_string()),
            Some(entry) => {
                if entry.tier != Tier::Lead {
                    reasons.push("judge must be Lead".to_string());
                }
                if entry.cost == Cost::FreeTrainsInput {
                    reasons.push(
                        "free-trains-input is not allowed for proprietary adversarial artifacts"
                            .to_string(),
                    );
                }
                if reviewer_chain
                    .iter()
                    .any(|reviewer| same_dispatch_identity(reviewer, entry))
                {
                    reasons.push("judge duplicates an approved reviewer identity".to_string());
                }
            }
        }
        match decision {
            None => reasons.push("judge provider is absent from the trusted snapshot".to_string()),
            Some(decision) if !decision_is_eligible(decision) => {
                reasons.push("judge provider is exhausted or unknown".to_string());
            }
            Some(_) => {}
        }
        if reasons.is_empty() {
            eligible.push(entry.expect("eligible judge has roster entry"));
        }
        audit.push(PanelCandidateAudit {
            role: "judge".to_string(),
            model: name.clone(),
            provider,
            availability: decision.map_or_else(|| "unknown".to_string(), decision_availability),
            outcome: if reasons.is_empty() {
                "eligible".to_string()
            } else {
                "excluded".to_string()
            },
            reasons,
        });
    }
    let Some(selected) = eligible.first() else {
        return Err(AdversarialError::new(
            "judge shortfall: no approved Lead judge remains eligible",
        ));
    };
    let fallbacks = eligible
        .iter()
        .skip(1)
        .filter(|entry| !same_dispatch_identity(selected, entry))
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    mark_audit_outcome(&mut audit, "judge", &selected.name, "selected-judge");
    add_audit_reason(
        &mut audit,
        "judge",
        &selected.name,
        "first eligible non-reviewer Lead in the configured judge chain",
    );
    for fallback in &fallbacks {
        mark_audit_outcome(&mut audit, "judge", fallback, "approved-judge-fallback");
        add_audit_reason(
            &mut audit,
            "judge",
            fallback,
            "later eligible Lead in the configured judge chain",
        );
    }
    Ok((
        JudgeSlot {
            model: selected.name.clone(),
            provider: bursar::normalize_provider_key(&selected.provider),
            fallbacks,
        },
        audit,
    ))
}

fn candidate_key(candidate: &EligibleCandidate<'_>) -> (u8, u8, u8, usize) {
    (
        cost_rank(candidate.entry.cost),
        reviewer_tier_rank(candidate.entry.tier),
        efficiency_rank(candidate.entry.efficiency),
        candidate.roster_index,
    )
}

fn cost_rank(cost: Cost) -> u8 {
    match cost {
        Cost::Free => 0,
        Cost::Paid => 1,
        Cost::FreeTrainsInput => 2,
    }
}

fn reviewer_tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Lead => 0,
        Tier::Senior => 1,
        Tier::Junior => 2,
    }
}

fn efficiency_rank(efficiency: Efficiency) -> u8 {
    match efficiency {
        Efficiency::Lean => 0,
        Efficiency::Std => 1,
        Efficiency::Heavy => 2,
    }
}

fn health_rank(action: BudgetAction) -> u8 {
    match action {
        BudgetAction::Proceed | BudgetAction::StaticCaps => 0,
        BudgetAction::SpendCautiously => 1,
        BudgetAction::Defer => 2,
    }
}

fn same_dispatch_identity(left: &RosterEntry, right: &RosterEntry) -> bool {
    left.backend == right.backend
        && left.dispatch_id == right.dispatch_id
        && left.reasoning_effort == right.reasoning_effort
}

fn decision_availability(decision: &BudgetDecision) -> String {
    decision.availability.map_or_else(
        || decision.action.label().to_string(),
        |availability| availability.to_string(),
    )
}

fn decision_is_eligible(decision: &BudgetDecision) -> bool {
    matches!(
        (decision.action, decision.availability),
        (BudgetAction::Proceed, Some(Availability::Healthy))
            | (BudgetAction::SpendCautiously, Some(Availability::Caution))
            | (BudgetAction::StaticCaps, None)
    )
}

fn mark_audit_outcome(audit: &mut [PanelCandidateAudit], role: &str, model: &str, outcome: &str) {
    if let Some(row) = audit
        .iter_mut()
        .find(|row| row.role == role && row.model == model)
    {
        row.outcome = outcome.to_string();
    }
}

fn add_audit_reason(audit: &mut [PanelCandidateAudit], role: &str, model: &str, reason: &str) {
    if let Some(row) = audit
        .iter_mut()
        .find(|row| row.role == role && row.model == model)
    {
        row.reasons.push(reason.to_string());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactRecord {
    source_path: String,
    snapshot_file: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactPlan {
    schema: String,
    review_id: String,
    artifact: ArtifactRecord,
}

#[derive(Serialize)]
struct EmptyProviderSnapshot<'a> {
    schema: &'a str,
    providers: [String; 0],
}

#[derive(Serialize)]
struct LifecycleState<'a> {
    schema: &'a str,
    status: &'a str,
}

pub(crate) fn snapshot_artifact(
    artifact_path: &Path,
    state_root: &Path,
    review_id: &str,
) -> Result<ArtifactSnapshot> {
    validate_review_id(review_id)?;
    let (source_path, bytes) = read_artifact(artifact_path)?;
    let size_bytes = u64::try_from(bytes.len())
        .map_err(|_| AdversarialError::new("artifact byte length does not fit u64"))?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let review_dir = state_root.join(review_id);
    if review_dir.exists() {
        return Err(AdversarialError::new(format!(
            "review state already exists: {}",
            review_dir.display()
        )));
    }
    fs::create_dir_all(state_root)
        .map_err(|error| AdversarialError::io("failed to create state root", state_root, &error))?;
    let temp_dir = create_temp_review_dir(state_root, review_id)?;
    let plan = ArtifactPlan {
        schema: "conductor-adversarial-plan-v1".to_string(),
        review_id: review_id.to_string(),
        artifact: ArtifactRecord {
            source_path: source_path
                .to_str()
                .ok_or_else(|| AdversarialError::new("canonical artifact path is not UTF-8"))?
                .to_string(),
            snapshot_file: ARTIFACT_FILE.to_string(),
            sha256: sha256.clone(),
            size_bytes,
        },
    };
    let write_result = (|| {
        write_new_file(&temp_dir.join(ARTIFACT_FILE), &bytes)?;
        write_new_file(
            &temp_dir.join("artifact.sha256"),
            format!("{sha256}\n").as_bytes(),
        )?;
        write_json(&temp_dir.join("plan.json"), &plan)?;
        write_json(
            &temp_dir.join("provider-snapshot.json"),
            &EmptyProviderSnapshot {
                schema: "conductor-adversarial-provider-snapshot-v1",
                providers: [],
            },
        )?;
        write_json(
            &temp_dir.join("lifecycle.json"),
            &LifecycleState {
                schema: "conductor-adversarial-lifecycle-v1",
                status: "artifact-snapshotted",
            },
        )?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }
    if review_dir.exists() {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(AdversarialError::new(format!(
            "review state already exists: {}",
            review_dir.display()
        )));
    }
    if let Err(error) = fs::rename(&temp_dir, &review_dir) {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(AdversarialError::io(
            "failed to publish review state",
            &review_dir,
            &error,
        ));
    }

    Ok(ArtifactSnapshot {
        source_path,
        snapshot_path: review_dir.join(ARTIFACT_FILE),
        review_dir,
        sha256,
        size_bytes,
    })
}

fn read_artifact(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    if contains_ai_scratch(path) {
        return Err(AdversarialError::new(
            "artifact path contains a forbidden ai-scratch component",
        ));
    }
    let initial = fs::symlink_metadata(path)
        .map_err(|error| AdversarialError::io("failed to inspect artifact", path, &error))?;
    if initial.file_type().is_symlink() {
        return Err(AdversarialError::new("artifact must not be a symlink"));
    }
    require_regular_readable_file(path, &initial)?;
    let canonical = fs::canonicalize(path)
        .map_err(|error| AdversarialError::io("failed to canonicalize artifact", path, &error))?;
    if contains_ai_scratch(&canonical) {
        return Err(AdversarialError::new(
            "canonical artifact path contains a forbidden ai-scratch component",
        ));
    }

    let mut file = File::open(&canonical)
        .map_err(|error| AdversarialError::io("failed to open artifact", &canonical, &error))?;
    let opened = file.metadata().map_err(|error| {
        AdversarialError::io("failed to inspect opened artifact", &canonical, &error)
    })?;
    let current = fs::symlink_metadata(&canonical).map_err(|error| {
        AdversarialError::io(
            "failed to re-inspect canonical artifact",
            &canonical,
            &error,
        )
    })?;
    if current.file_type().is_symlink() {
        return Err(AdversarialError::new(
            "artifact became a symlink while being opened",
        ));
    }
    require_regular_readable_file(&canonical, &opened)?;
    if !same_file(&opened, &current) {
        return Err(AdversarialError::new(
            "artifact identity changed while being opened",
        ));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    (&mut file)
        .take(u64::try_from(MAX_ARTIFACT_BYTES + 1).expect("artifact limit fits u64"))
        .read_to_end(&mut bytes)
        .map_err(|error| AdversarialError::io("failed to read artifact", &canonical, &error))?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(AdversarialError::new(format!(
            "artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    let after = file.metadata().map_err(|error| {
        AdversarialError::io("failed to re-inspect artifact", &canonical, &error)
    })?;
    if !same_file(&opened, &after)
        || after.len() != opened.len()
        || after.len() != bytes.len() as u64
    {
        return Err(AdversarialError::new(
            "artifact changed while its immutable snapshot was being read",
        ));
    }
    Ok((canonical, bytes))
}

fn require_regular_readable_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        return Err(AdversarialError::new("artifact must be a regular file"));
    }
    if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
        return Err(AdversarialError::new(format!(
            "artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o444 == 0 {
            return Err(AdversarialError::new(format!(
                "artifact is not readable: {}",
                path.display()
            )));
        }
        if metadata.nlink() > 1 {
            return Err(AdversarialError::new(format!(
                "artifact has multiple hard links and cannot be proven outside ai-scratch: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn contains_ai_scratch(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name == OsStr::new("ai-scratch")
                    || name.to_str().is_some_and(|name| name.eq_ignore_ascii_case("ai-scratch"))
        )
    })
}

fn validate_review_id(review_id: &str) -> Result<()> {
    let mut bytes = review_id.bytes();
    let valid = !review_id.is_empty()
        && review_id.len() <= MAX_REVIEW_ID_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AdversarialError::new(format!(
            "invalid review id {review_id:?}; expected an alphanumeric prefix followed by alphanumeric, '_' or '-' bytes"
        )))
    }
}

fn create_temp_review_dir(state_root: &Path, review_id: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0_u8..100 {
        let path = state_root.join(format!(
            ".{review_id}.{}-{nanos}-{attempt}.tmp",
            std::process::id()
        ));
        let created = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700).create(&path)
            }
            #[cfg(not(unix))]
            {
                fs::create_dir(&path)
            }
        };
        match created {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(AdversarialError::io(
                    "failed to create temporary review state",
                    &path,
                    &error,
                ));
            }
        }
    }
    Err(AdversarialError::new(
        "failed to allocate a unique temporary review state directory",
    ))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| AdversarialError::io("failed to create state file", path, &error))?;
    file.write_all(bytes)
        .map_err(|error| AdversarialError::io("failed to write state file", path, &error))?;
    file.sync_all()
        .map_err(|error| AdversarialError::io("failed to sync state file", path, &error))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AdversarialError::new(format!("failed to serialize state: {error}")))?;
    bytes.push(b'\n');
    write_new_file(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bursar::Availability;
    use crate::config::{Backend, Ceiling};
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn panel_distinct_providers_health_and_deterministic_ordering() {
        let roster = vec![
            panel_roster_entry(
                "p1-paid-lead",
                Tier::Lead,
                "provider-one",
                Cost::Paid,
                Efficiency::Lean,
                "p1-paid-lead",
            ),
            panel_roster_entry(
                "p1-free-senior",
                Tier::Senior,
                "provider-one",
                Cost::Free,
                Efficiency::Lean,
                "p1-free-senior",
            ),
            panel_roster_entry(
                "p1-free-lead",
                Tier::Lead,
                "provider-one",
                Cost::Free,
                Efficiency::Heavy,
                "p1-free-lead",
            ),
            panel_roster_entry(
                "caution-free-lead",
                Tier::Lead,
                "provider-two",
                Cost::Free,
                Efficiency::Lean,
                "caution-free-lead",
            ),
            panel_roster_entry(
                "healthy-paid-senior",
                Tier::Senior,
                "provider-three",
                Cost::Paid,
                Efficiency::Std,
                "healthy-paid-senior",
            ),
            panel_roster_entry(
                "judge",
                Tier::Lead,
                "provider-two",
                Cost::Paid,
                Efficiency::Heavy,
                "judge",
            ),
        ];
        let config = panel_config("judge", &[]);
        let providers = panel_provider_snapshot(&[
            ("provider-one", Availability::Healthy),
            ("provider-two", Availability::Caution),
            ("provider-three", Availability::Healthy),
        ]);

        let panel = plan_panel(&roster, &config, &providers, 2, None).expect("panel plans");

        assert_eq!(
            panel
                .reviewers
                .iter()
                .map(|slot| slot.provider.as_str())
                .collect::<Vec<_>>(),
            ["provider-one", "provider-three"]
        );
        assert_eq!(panel.reviewers[0].model, "p1-free-lead");
        assert_eq!(
            panel.reviewers[0].alternatives,
            ["p1-free-senior", "p1-paid-lead"]
        );
        let selected_audit = panel
            .audit
            .iter()
            .find(|row| row.role == "reviewer" && row.model == "p1-free-lead")
            .expect("selected reviewer audited");
        assert!(!selected_audit.reasons.is_empty());
        assert_eq!(panel.judge.model, "judge");
    }

    #[test]
    fn panel_shortfall_never_duplicates_a_provider() {
        let roster = vec![
            panel_roster_entry(
                "one-a",
                Tier::Senior,
                "provider-one",
                Cost::Paid,
                Efficiency::Lean,
                "one-a",
            ),
            panel_roster_entry(
                "one-b",
                Tier::Lead,
                "provider-one",
                Cost::Paid,
                Efficiency::Std,
                "one-b",
            ),
            panel_roster_entry(
                "judge",
                Tier::Lead,
                "provider-two",
                Cost::Paid,
                Efficiency::Heavy,
                "judge",
            ),
        ];
        let providers = panel_provider_snapshot(&[
            ("provider-one", Availability::Healthy),
            ("provider-two", Availability::Healthy),
        ]);

        let error = plan_panel(&roster, &panel_config("judge", &[]), &providers, 3, None)
            .expect_err("two providers cannot fill three reviewer slots");
        assert!(error.to_string().contains("provider shortfall"));
        assert!(error.to_string().contains("only 2"));
    }

    #[test]
    fn panel_explicit_models_cannot_bypass_closed_roster_health_tier_or_distinctness() {
        let roster = vec![
            panel_roster_entry(
                "one-a",
                Tier::Senior,
                "provider-one",
                Cost::Paid,
                Efficiency::Lean,
                "one-a",
            ),
            panel_roster_entry(
                "one-b",
                Tier::Lead,
                "provider-one",
                Cost::Paid,
                Efficiency::Std,
                "one-b",
            ),
            panel_roster_entry(
                "junior",
                Tier::Junior,
                "provider-two",
                Cost::Free,
                Efficiency::Lean,
                "junior",
            ),
            panel_roster_entry(
                "exhausted",
                Tier::Senior,
                "provider-three",
                Cost::Paid,
                Efficiency::Lean,
                "exhausted",
            ),
            panel_roster_entry(
                "judge",
                Tier::Lead,
                "provider-four",
                Cost::Paid,
                Efficiency::Heavy,
                "judge",
            ),
        ];
        let providers = panel_provider_snapshot(&[
            ("provider-one", Availability::Healthy),
            ("provider-two", Availability::Healthy),
            ("provider-three", Availability::Exhausted),
            ("provider-four", Availability::Healthy),
        ]);
        let config = panel_config("judge", &[]);
        for models in [
            vec!["one-a".to_string()],
            vec!["one-a".to_string(), "missing".to_string()],
            vec!["one-a".to_string(), "one-b".to_string()],
            vec!["one-a".to_string(), "junior".to_string()],
            vec!["one-a".to_string(), "exhausted".to_string()],
        ] {
            assert!(
                plan_panel(&roster, &config, &providers, 2, Some(&models)).is_err(),
                "explicit gate bypassed for {models:?}"
            );
        }
    }

    #[test]
    fn panel_judge_excludes_reviewer_identity_and_uses_configured_lead_chain() {
        let roster = vec![
            panel_roster_entry(
                "reviewer",
                Tier::Senior,
                "provider-one",
                Cost::Paid,
                Efficiency::Lean,
                "shared-dispatch",
            ),
            panel_roster_entry(
                "judge-alias",
                Tier::Lead,
                "provider-two",
                Cost::Paid,
                Efficiency::Heavy,
                "shared-dispatch",
            ),
            panel_roster_entry(
                "judge-fallback",
                Tier::Lead,
                "provider-three",
                Cost::Paid,
                Efficiency::Std,
                "judge-fallback",
            ),
        ];
        let providers = panel_provider_snapshot(&[
            ("provider-one", Availability::Healthy),
            ("provider-two", Availability::Healthy),
            ("provider-three", Availability::Caution),
        ]);
        let explicit = vec!["reviewer".to_string()];

        let panel = plan_panel(
            &roster,
            &panel_config("judge-alias", &["judge-fallback"]),
            &providers,
            1,
            Some(&explicit),
        )
        .expect("fallback judge remains eligible");

        assert_eq!(panel.judge.model, "judge-fallback");
        assert!(panel.judge.fallbacks.is_empty());
        let alias = panel
            .audit
            .iter()
            .find(|row| row.role == "judge" && row.model == "judge-alias")
            .expect("judge alias audited");
        assert_eq!(alias.outcome, "excluded");
        assert!(
            alias
                .reasons
                .iter()
                .any(|reason| reason.contains("duplicates"))
        );
    }

    #[test]
    fn panel_audit_retains_provider_and_data_policy_exclusions() {
        let roster = vec![
            panel_roster_entry(
                "selected",
                Tier::Senior,
                "provider-one",
                Cost::Paid,
                Efficiency::Lean,
                "selected",
            ),
            panel_roster_entry(
                "training",
                Tier::Lead,
                "provider-two",
                Cost::FreeTrainsInput,
                Efficiency::Lean,
                "training",
            ),
            panel_roster_entry(
                "unknown",
                Tier::Senior,
                "provider-three",
                Cost::Paid,
                Efficiency::Lean,
                "unknown",
            ),
            panel_roster_entry(
                "judge",
                Tier::Lead,
                "provider-four",
                Cost::Paid,
                Efficiency::Heavy,
                "judge",
            ),
        ];
        let providers = panel_provider_snapshot(&[
            ("provider-one", Availability::Healthy),
            ("provider-two", Availability::Healthy),
            ("provider-three", Availability::Unknown),
            ("provider-four", Availability::Healthy),
        ]);
        let explicit = vec!["selected".to_string()];

        let panel = plan_panel(
            &roster,
            &panel_config("judge", &[]),
            &providers,
            1,
            Some(&explicit),
        )
        .expect("one valid reviewer and judge");

        for model in ["training", "unknown"] {
            let row = panel
                .audit
                .iter()
                .find(|row| row.role == "reviewer" && row.model == model)
                .expect("candidate audited");
            assert_eq!(row.outcome, "excluded");
            assert!(!row.reasons.is_empty());
        }
    }

    #[test]
    fn artifact_snapshot_preserves_exact_bytes_hash_and_atomic_state() {
        let temp = TempDir::new("artifact-exact");
        let artifact = temp.path().join("decision.bin");
        let bytes = b"line one\r\n\0\xffline two\n";
        std::fs::write(&artifact, bytes).expect("write artifact");

        let snapshot = snapshot_artifact(&artifact, &temp.path().join("state"), "review-exact")
            .expect("snapshot accepted artifact");

        assert_eq!(snapshot.size_bytes, bytes.len() as u64);
        assert_eq!(snapshot.sha256, format!("{:x}", Sha256::digest(bytes)));
        assert_eq!(std::fs::read(&snapshot.snapshot_path).unwrap(), bytes);
        assert_eq!(
            snapshot.source_path,
            std::fs::canonicalize(&artifact).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(snapshot.review_dir.join("artifact.sha256")).unwrap(),
            format!("{}\n", snapshot.sha256)
        );
        for file in ["plan.json", "provider-snapshot.json", "lifecycle.json"] {
            assert!(snapshot.review_dir.join(file).is_file(), "missing {file}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&snapshot.review_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&snapshot.snapshot_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(snapshot.review_dir.join("plan.json")).unwrap())
                .unwrap();
        assert_eq!(plan["artifact"]["sha256"], snapshot.sha256);
        assert_eq!(plan["artifact"]["size_bytes"], bytes.len() as u64);
    }

    #[test]
    fn artifact_rejects_directory_oversize_and_ai_scratch_component() {
        let temp = TempDir::new("artifact-boundaries");
        let state = temp.path().join("state");
        assert!(snapshot_artifact(temp.path(), &state, "review-directory").is_err());

        let oversized = temp.path().join("oversized.bin");
        std::fs::write(&oversized, vec![0_u8; MAX_ARTIFACT_BYTES + 1]).unwrap();
        assert!(snapshot_artifact(&oversized, &state, "review-oversized").is_err());
        assert!(!state.join("review-oversized").exists());

        let scratch = temp.path().join("AI-SCRATCH");
        std::fs::create_dir(&scratch).unwrap();
        let scratch_artifact = scratch.join("decision.md");
        std::fs::write(&scratch_artifact, b"secret").unwrap();
        assert!(snapshot_artifact(&scratch_artifact, &state, "review-scratch").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_rejects_symlinks_unreadable_files_and_canonical_ai_scratch() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new("artifact-unix-boundaries");
        let state = temp.path().join("state");
        let target = temp.path().join("target.md");
        std::fs::write(&target, b"target").unwrap();
        let link = temp.path().join("link.md");
        symlink(&target, &link).unwrap();
        assert!(snapshot_artifact(&link, &state, "review-symlink").is_err());

        let unreadable = temp.path().join("unreadable.md");
        std::fs::write(&unreadable, b"closed").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = snapshot_artifact(&unreadable, &state, "review-unreadable");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err());

        let scratch = temp.path().join("ai-scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("decision.md"), b"secret").unwrap();
        let alias = temp.path().join("alias");
        symlink(&scratch, &alias).unwrap();
        assert!(
            snapshot_artifact(
                &alias.join("decision.md"),
                &state,
                "review-canonical-scratch"
            )
            .is_err()
        );

        let hard_link = temp.path().join("hard-link-alias.md");
        std::fs::hard_link(scratch.join("decision.md"), &hard_link).unwrap();
        assert!(snapshot_artifact(&hard_link, &state, "review-hard-link").is_err());
    }

    #[test]
    fn artifact_rejects_invalid_or_reused_review_id() {
        let temp = TempDir::new("artifact-review-id");
        let artifact = temp.path().join("decision.md");
        let state = temp.path().join("state");
        std::fs::write(&artifact, b"decision").unwrap();
        assert!(snapshot_artifact(&artifact, &state, "../escape").is_err());
        snapshot_artifact(&artifact, &state, "review-once").expect("first snapshot");
        assert!(snapshot_artifact(&artifact, &state, "review-once").is_err());
    }

    fn panel_roster_entry(
        name: &str,
        tier: Tier,
        provider: &str,
        cost: Cost,
        efficiency: Efficiency,
        dispatch_id: &str,
    ) -> RosterEntry {
        RosterEntry {
            name: name.to_string(),
            tier,
            ceiling: Ceiling::Xl,
            efficiency,
            backend: Backend::Pi,
            dispatch_id: dispatch_id.to_string(),
            reasoning_effort: None,
            provider: provider.to_string(),
            cost,
            fallback: Vec::new(),
        }
    }

    fn panel_config(judge: &str, fallbacks: &[&str]) -> AdversarialReviewConfig {
        AdversarialReviewConfig {
            max_reviewers: 7,
            parallel: 3,
            judge: judge.to_string(),
            judge_fallbacks: fallbacks
                .iter()
                .map(|fallback| (*fallback).to_string())
                .collect(),
        }
    }

    fn panel_provider_snapshot(
        providers: &[(&str, Availability)],
    ) -> BTreeMap<String, BudgetDecision> {
        providers
            .iter()
            .map(|(provider, availability)| {
                let action = match availability {
                    Availability::Healthy => BudgetAction::Proceed,
                    Availability::Caution => BudgetAction::SpendCautiously,
                    Availability::Exhausted | Availability::Unknown => BudgetAction::Defer,
                };
                (
                    bursar::normalize_provider_key(provider),
                    BudgetDecision {
                        provider: bursar::normalize_provider_key(provider),
                        model: None,
                        availability: Some(*availability),
                        source: Some("test".to_string()),
                        checked_at: None,
                        data_as_of: None,
                        expires_at: None,
                        expiry_basis: None,
                        action,
                        summary: "test provider state".to_string(),
                    },
                )
            })
            .collect()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("conductor-{label}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
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
