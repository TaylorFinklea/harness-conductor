//! Bursar `status --json` client and budget decision helpers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::process::{Command, Stdio};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "bursar/status@2";
const ROSTER_SCHEMA: &str = "bursar/roster@1";
const MAX_STATUS_AGE_MINS: i64 = 5;
const NEAR_EXHAUSTED_PERCENT: f64 = 90.0;
const PROVIDERS: [&str; 4] = ["anthropic", "codex", "opencode-go", "agy"];

pub(crate) type Result<T> = std::result::Result<T, BursarError>;

pub(crate) trait BursarClient {
    fn status(&self) -> Result<StatusReport>;

    /// Read the authoritative, resolved Bursar execution-profile snapshot.
    /// Implementors without this newer seam remain read-only legacy clients;
    /// callers must fail closed rather than fabricating a roster.
    fn roster_snapshot(&self) -> Result<RosterSnapshot> {
        Err(BursarError::unavailable(
            "bursar roster snapshot unavailable",
        ))
    }

    #[allow(dead_code)]
    fn observe(&self, _request: &ObservationRequest) -> Result<()> {
        Err(BursarError::unavailable("bursar observation unavailable"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BursarErrorKind {
    Unavailable,
    Command,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BursarError {
    kind: BursarErrorKind,
    message: String,
}

impl BursarError {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: BursarErrorKind::Unavailable,
            message: message.into(),
        }
    }

    fn command(message: impl Into<String>) -> Self {
        Self {
            kind: BursarErrorKind::Command,
            message: message.into(),
        }
    }

    fn json(message: impl Into<String>) -> Self {
        Self {
            kind: BursarErrorKind::Json,
            message: message.into(),
        }
    }

    pub(crate) const fn is_unavailable(&self) -> bool {
        matches!(self.kind, BursarErrorKind::Unavailable)
    }
}

impl fmt::Display for BursarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BursarError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Availability {
    Healthy,
    Caution,
    Exhausted,
    Unknown,
}

impl fmt::Display for Availability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::Caution => f.write_str("caution"),
            Self::Exhausted => f.write_str("exhausted"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct Window {
    pub(crate) label: String,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) percent: Option<f64>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderStatus {
    pub(crate) availability: Availability,
    pub(crate) source: String,
    pub(crate) checked_at: String,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) data_as_of: Option<String>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) expires_at: Option<String>,
    pub(crate) windows: Vec<Window>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) reason: Option<String>,
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusReport {
    pub(crate) schema: String,
    pub(crate) checked_at: String,
    pub(crate) providers: BTreeMap<String, ProviderStatus>,
}

/// Immutable Bursar roster artifact identity persisted with an approved plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterArtifact {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// The read-only `bursar/roster@1` response consumed by Conductor.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterSnapshot {
    schema: String,
    generated_at: String,
    pub(crate) artifact: RosterArtifact,
    providers: Vec<RosterProvider>,
    profiles: Vec<RosterProfile>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterProvider {
    provider_id: String,
    availability_key: String,
    enabled: bool,
    state: String,
    availability: Availability,
    checked_at: String,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    data_as_of: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    expires_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    reason: Option<String>,
    eligible: bool,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    ineligibility_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterProfile {
    profile_id: String,
    provider_id: String,
    model: String,
    harness: String,
    dispatch_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    reasoning_effort: Option<String>,
    tier: String,
    ceiling: String,
    efficiency: String,
    cost: f64,
    data_policy: String,
    enabled: bool,
    state: String,
    eligible: bool,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    ineligibility_reason: Option<String>,
}

/// Parses, authenticates, and validates a `bursar/roster@1` snapshot before
/// any profile becomes eligible for Conductor routing.
pub(crate) fn parse_roster_snapshot(bytes: &[u8]) -> Result<RosterSnapshot> {
    let snapshot: RosterSnapshot = serde_json::from_slice(bytes).map_err(|error| {
        BursarError::json(format!("failed to parse bursar roster snapshot: {error}"))
    })?;
    snapshot.validate()?;
    Ok(snapshot)
}

impl RosterSnapshot {
    #[expect(
        clippy::too_many_lines,
        reason = "linear fail-closed validation keeps each evidence rejection explicit"
    )]
    fn validate(&self) -> Result<()> {
        if self.schema != ROSTER_SCHEMA {
            return Err(BursarError::json(format!(
                "unsupported bursar roster schema {}",
                self.schema
            )));
        }
        parse_time("roster generated_at", &self.generated_at)
            .map_err(|error| BursarError::json(error.clone()))?;
        let artifact_path = std::path::Path::new(&self.artifact.path);
        if !artifact_path.is_absolute() {
            return Err(BursarError::json(
                "bursar roster artifact path must be absolute",
            ));
        }
        if !is_sha256(&self.artifact.sha256) {
            return Err(BursarError::json(
                "bursar roster artifact sha256 must be lowercase 64-hex",
            ));
        }
        let bytes = std::fs::read(artifact_path).map_err(|error| {
            BursarError::json(format!(
                "cannot read bursar roster artifact {}: {error}",
                artifact_path.display()
            ))
        })?;
        let actual_sha256 = format!("{:x}", Sha256::digest(bytes));
        if actual_sha256 != self.artifact.sha256 {
            return Err(BursarError::json(format!(
                "bursar roster artifact hash mismatch for {}",
                artifact_path.display()
            )));
        }

        let mut providers = HashMap::new();
        for provider in &self.providers {
            if provider.provider_id.is_empty()
                || provider.availability_key.is_empty()
                || !matches!(
                    provider.state.as_str(),
                    "healthy" | "exhausted" | "unknown" | "stale" | "manually-disabled"
                )
            {
                return Err(BursarError::json("malformed bursar roster provider"));
            }
            parse_time("roster provider checked_at", &provider.checked_at)
                .map_err(|error| BursarError::json(error.clone()))?;
            if let Some(data_as_of) = provider.data_as_of.as_deref() {
                parse_time("roster provider data_as_of", data_as_of)
                    .map_err(|error| BursarError::json(error.clone()))?;
            }
            if let Some(expires_at) = provider.expires_at.as_deref() {
                parse_time("roster provider expires_at", expires_at)
                    .map_err(|error| BursarError::json(error.clone()))?;
            }
            if provider.eligible
                && (!provider.enabled
                    || provider.state != "healthy"
                    || !matches!(
                        provider.availability,
                        Availability::Healthy | Availability::Caution
                    ))
            {
                return Err(BursarError::json(
                    "eligible bursar roster provider is disabled or unavailable",
                ));
            }
            if providers
                .insert(provider.provider_id.as_str(), provider)
                .is_some()
            {
                return Err(BursarError::json("duplicate bursar roster provider_id"));
            }
        }

        let mut profile_ids = HashSet::new();
        for profile in &self.profiles {
            if profile.profile_id.is_empty()
                || profile.provider_id.is_empty()
                || profile.model.is_empty()
                || profile.dispatch_id.is_empty()
                || !providers.contains_key(profile.provider_id.as_str())
                || !profile.cost.is_finite()
                || profile.cost < 0.0
                || !matches!(
                    profile.state.as_str(),
                    "healthy" | "exhausted" | "unknown" | "stale" | "manually-disabled"
                )
                || !matches!(
                    profile.data_policy.as_str(),
                    "standard" | "zero-retention" | "local-only" | "trains-input"
                )
            {
                return Err(BursarError::json("malformed bursar roster profile"));
            }
            profile
                .tier
                .parse::<crate::config::Tier>()
                .map_err(|error| {
                    BursarError::json(format!("malformed bursar roster profile tier: {error}"))
                })?;
            profile
                .ceiling
                .parse::<crate::config::Ceiling>()
                .map_err(|error| {
                    BursarError::json(format!("malformed bursar roster profile ceiling: {error}"))
                })?;
            profile
                .efficiency
                .parse::<crate::config::Efficiency>()
                .map_err(|error| {
                    BursarError::json(format!(
                        "malformed bursar roster profile efficiency: {error}"
                    ))
                })?;
            backend_from_harness(&profile.harness)?;
            if let Some(reasoning_effort) = profile.reasoning_effort.as_deref() {
                reasoning_effort
                    .parse::<crate::config::ReasoningEffort>()
                    .map_err(|error| {
                        BursarError::json(format!(
                            "malformed bursar roster profile reasoning effort: {error}"
                        ))
                    })?;
            }
            let provider = providers
                .get(profile.provider_id.as_str())
                .expect("profile provider was checked above");
            if profile.eligible
                && (!profile.enabled
                    || profile.state != "healthy"
                    || !provider.enabled
                    || !provider.eligible)
            {
                return Err(BursarError::json(
                    "eligible bursar roster profile has a disabled or unavailable provider",
                ));
            }
            if !profile_ids.insert(profile.profile_id.as_str()) {
                return Err(BursarError::json("duplicate bursar roster profile_id"));
            }
        }
        Ok(())
    }

    /// Resolve Conductor-owned job fallback policy against this snapshot.
    /// Bursar profiles supply identity/capability/availability, while policy
    /// only orders already-known profile IDs after a runtime retryable error.
    pub(crate) fn roster_entries_with_fallbacks(
        &self,
        job_fallbacks: &[crate::config::JobFallbackPolicy],
    ) -> Result<Vec<crate::config::RosterEntry>> {
        let known_profile_ids = self
            .profiles
            .iter()
            .map(|profile| profile.profile_id.as_str())
            .collect::<HashSet<_>>();
        for policy in job_fallbacks {
            if !known_profile_ids.contains(policy.profile_id.as_str()) {
                return Err(BursarError::json(format!(
                    "Conductor job fallback references missing Bursar profile {}",
                    policy.profile_id
                )));
            }
            for fallback in &policy.fallback_profile_ids {
                if !known_profile_ids.contains(fallback.as_str()) {
                    return Err(BursarError::json(format!(
                        "Conductor job fallback for {} references missing Bursar profile {fallback}",
                        policy.profile_id
                    )));
                }
            }
        }

        let eligible_profile_ids = self
            .profiles
            .iter()
            .filter(|profile| profile.enabled && profile.eligible)
            .map(|profile| profile.profile_id.as_str())
            .collect::<HashSet<_>>();
        let mut entries = self
            .profiles
            .iter()
            .filter(|profile| profile.enabled && profile.eligible)
            .map(|profile| {
                let backend = backend_from_harness(&profile.harness)?;
                let reasoning_effort = profile
                    .reasoning_effort
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .map_err(|error| {
                        BursarError::json(format!(
                            "malformed bursar roster profile reasoning effort: {error}"
                        ))
                    })?;
                let cost = if profile.data_policy == "trains-input" {
                    crate::config::Cost::FreeTrainsInput
                } else if profile.cost == 0.0 {
                    crate::config::Cost::Free
                } else {
                    crate::config::Cost::Paid
                };
                Ok(crate::config::RosterEntry {
                    name: profile.profile_id.clone(),
                    tier: profile.tier.parse().map_err(|error| {
                        BursarError::json(format!("malformed bursar roster profile tier: {error}"))
                    })?,
                    ceiling: profile.ceiling.parse().map_err(|error| {
                        BursarError::json(format!(
                            "malformed bursar roster profile ceiling: {error}"
                        ))
                    })?,
                    efficiency: profile.efficiency.parse().map_err(|error| {
                        BursarError::json(format!(
                            "malformed bursar roster profile efficiency: {error}"
                        ))
                    })?,
                    backend,
                    dispatch_id: profile.dispatch_id.clone(),
                    reasoning_effort,
                    provider: profile.provider_id.clone(),
                    cost,
                    fallback: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        for policy in job_fallbacks {
            let Some(entry) = entries
                .iter_mut()
                .find(|entry| entry.name == policy.profile_id)
            else {
                continue;
            };
            entry.fallback = policy
                .fallback_profile_ids
                .iter()
                .filter(|profile_id| eligible_profile_ids.contains(profile_id.as_str()))
                .cloned()
                .collect();
        }
        Ok(entries)
    }
}

fn backend_from_harness(harness: &str) -> Result<crate::config::Backend> {
    match harness {
        "claude-code" => Ok(crate::config::Backend::Claude),
        "pi" => Ok(crate::config::Backend::Pi),
        "agy" => Ok(crate::config::Backend::Agy),
        "codex" => Ok(crate::config::Backend::Codex),
        _ => Err(BursarError::json(format!(
            "unsupported bursar roster harness {harness:?}"
        ))),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Runtime roster resolution. Production configs carry no `[[roster]]` rows,
/// so a missing snapshot is a launch-stopping error. Explicit legacy rows are
/// retained only as a read-only compatibility bridge for persisted plans and
/// fixtures until the migration window closes.
pub(crate) fn resolve_roster<C: BursarClient + ?Sized>(
    cfg: &crate::config::Config,
    client: &C,
) -> Result<ResolvedRoster> {
    match client.roster_snapshot() {
        Ok(snapshot) => Ok(ResolvedRoster {
            roster: snapshot.roster_entries_with_fallbacks(&cfg.job_fallbacks)?,
            artifact: Some(snapshot.artifact),
        }),
        Err(_error) if !cfg.roster.is_empty() => Ok(ResolvedRoster {
            roster: cfg.roster.clone(),
            artifact: None,
        }),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedRoster {
    pub(crate) roster: Vec<crate::config::RosterEntry>,
    pub(crate) artifact: Option<RosterArtifact>,
}

fn deserialize_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetAction {
    Proceed,
    SpendCautiously,
    Defer,
    StaticCaps,
}

impl BudgetAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::SpendCautiously => "spend-cautiously",
            Self::Defer => "defer",
            Self::StaticCaps => "static-caps",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetDecision {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) availability: Option<Availability>,
    pub(crate) source: Option<String>,
    pub(crate) checked_at: Option<String>,
    pub(crate) data_as_of: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) expiry_basis: Option<String>,
    pub(crate) action: BudgetAction,
    pub(crate) summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ObservationExpiryBasis {
    ProviderReset,
    LocalCooldown,
}

impl ObservationExpiryBasis {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ProviderReset => "provider-reset",
            Self::LocalCooldown => "local-cooldown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RuntimeLimitReason {
    Http429,
    QuotaExceeded,
    RateLimit,
}

impl RuntimeLimitReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Http429 => "runtime HTTP 429",
            Self::QuotaExceeded => "runtime quota exceeded",
            Self::RateLimit => "runtime rate limit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservationRequest {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) expires_at: String,
    pub(crate) expiry_basis: ObservationExpiryBasis,
    pub(crate) reason: RuntimeLimitReason,
}

impl ObservationRequest {
    pub(crate) fn runtime_limit(
        provider: impl Into<String>,
        model: Option<String>,
        expires_at: impl Into<String>,
        expiry_basis: ObservationExpiryBasis,
        reason: RuntimeLimitReason,
    ) -> Self {
        let provider = provider.into();
        Self {
            provider: canonical_provider_key(&provider).to_string(),
            model,
            expires_at: expires_at.into(),
            expiry_basis,
            reason,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandBursarClient;

impl CommandBursarClient {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl BursarClient for CommandBursarClient {
    fn status(&self) -> Result<StatusReport> {
        let output = Command::new("bursar")
            .args(["status", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| spawn_error("bursar status --json", &error))?;

        if !output.status.success() {
            return Err(command_failure("bursar status --json", &output));
        }

        serde_json::from_slice(&output.stdout).map_err(|error| {
            BursarError::json(format!("failed to parse bursar status --json: {error}"))
        })
    }

    fn roster_snapshot(&self) -> Result<RosterSnapshot> {
        let output = Command::new("bursar")
            .args(["roster", "snapshot", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| spawn_error("bursar roster snapshot --json", &error))?;

        if !output.status.success() {
            return Err(command_failure("bursar roster snapshot --json", &output));
        }

        parse_roster_snapshot(&output.stdout)
    }

    fn observe(&self, request: &ObservationRequest) -> Result<()> {
        let args = observation_args(request);
        let output = Command::new("bursar")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| spawn_error("bursar observe", &error))?;

        if !output.status.success() {
            return Err(command_failure("bursar observe", &output));
        }
        Ok(())
    }
}

fn spawn_error(command: &str, error: &io::Error) -> BursarError {
    match error.kind() {
        io::ErrorKind::NotFound => BursarError::unavailable("bursar unavailable on PATH"),
        _ => BursarError::command(format!("failed to spawn {command}: {error}")),
    }
}

fn command_failure(command: &str, output: &std::process::Output) -> BursarError {
    let status = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    BursarError::command(format!("{command} exited {status}: {detail}"))
}

#[allow(dead_code)]
fn observation_args(request: &ObservationRequest) -> Vec<String> {
    let mut args = vec![
        "observe".to_string(),
        "--provider".to_string(),
        request.provider.clone(),
        "--availability".to_string(),
        "exhausted".to_string(),
        "--expires-at".to_string(),
        request.expires_at.clone(),
        "--expiry-basis".to_string(),
        request.expiry_basis.label().to_string(),
        "--source".to_string(),
        "conductor-runtime".to_string(),
        "--reason".to_string(),
        request.reason.label().to_string(),
    ];
    if let Some(model) = request.model.as_deref() {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    args
}

pub(crate) fn canonical_provider_key(provider: &str) -> &str {
    match provider {
        "openai-codex" => "codex",
        other => other,
    }
}

pub(crate) fn normalize_provider_key(provider: &str) -> String {
    canonical_provider_key(provider.trim()).to_ascii_lowercase()
}

#[derive(Clone)]
#[allow(dead_code)]
struct SnapshotBursarClient {
    result: Result<StatusReport>,
}

impl BursarClient for SnapshotBursarClient {
    fn status(&self) -> Result<StatusReport> {
        self.result.clone()
    }
}

#[allow(dead_code)]
pub(crate) fn evaluate_provider_snapshot<C, I, S>(
    client: &C,
    providers: I,
    use_bursar: bool,
) -> BTreeMap<String, BudgetDecision>
where
    C: BursarClient + ?Sized,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let result = if use_bursar {
        client.status()
    } else {
        Err(BursarError::unavailable(
            "bursar intentionally bypassed by static caps",
        ))
    };
    let snapshot = SnapshotBursarClient { result };
    providers
        .into_iter()
        .map(|provider| normalize_provider_key(provider.as_ref()))
        .filter(|provider| !provider.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|provider| {
            let decision = evaluate_budget(&snapshot, &provider, use_bursar);
            (provider, decision)
        })
        .collect()
}

pub(crate) fn evaluate_budget<C: BursarClient + ?Sized>(
    client: &C,
    provider: &str,
    use_bursar: bool,
) -> BudgetDecision {
    let result = if use_bursar {
        client.status()
    } else {
        Err(BursarError::unavailable(
            "bursar intentionally bypassed by static caps",
        ))
    };
    let snapshot = SnapshotBursarClient { result };
    evaluate_budget_at(&snapshot, provider, use_bursar, Utc::now())
}

#[expect(
    clippy::too_many_lines,
    reason = "linear fail-closed validation keeps each evidence rejection explicit"
)]
fn evaluate_budget_at<C: BursarClient + ?Sized>(
    client: &C,
    provider: &str,
    use_bursar: bool,
    now: DateTime<Utc>,
) -> BudgetDecision {
    let provider = canonical_provider_key(provider);
    if !use_bursar {
        return decision(
            provider,
            BudgetAction::StaticCaps,
            format!("{provider}: static-caps — budgets.use_bursar is false"),
        );
    }

    let report = match client.status() {
        Ok(report) => report,
        Err(error) if error.is_unavailable() => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — bursar unavailable ({error})"),
            );
        }
        Err(error) => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — bursar status error: {error}"),
            );
        }
    };

    if report.schema != SCHEMA {
        return decision(
            provider,
            BudgetAction::Defer,
            format!(
                "{provider}: defer — unsupported bursar schema {}",
                report.schema
            ),
        );
    }
    if !PROVIDERS
        .iter()
        .all(|provider| report.providers.contains_key(*provider))
    {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — bursar/status@2 missing baseline provider"),
        );
    }

    let report_checked_at = match parse_time("report checked_at", &report.checked_at) {
        Ok(value) => value,
        Err(error) => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — {error}"),
            );
        }
    };
    if report_checked_at > now {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — bursar report checked_at is in the future"),
        );
    }
    if now - report_checked_at > Duration::minutes(MAX_STATUS_AGE_MINS) {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — bursar report is stale"),
        );
    }

    let Some(status) = report.providers.get(provider) else {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — provider absent from bursar/status@2"),
        );
    };

    let status_checked_at = match parse_time("provider checked_at", &status.checked_at) {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    if status_checked_at != report_checked_at {
        return defer_with_status(
            provider,
            status,
            format!("{provider}: defer — provider checked_at does not match report checked_at"),
        );
    }

    if let Some(data_as_of) = status.data_as_of.as_deref() {
        let parsed = match parse_time("provider data_as_of", data_as_of) {
            Ok(value) => value,
            Err(error) => {
                return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
            }
        };
        if parsed > report_checked_at {
            return defer_with_status(
                provider,
                status,
                format!("{provider}: defer — provider data_as_of is in the future"),
            );
        }
    }

    if let Some(expires_at) = status.expires_at.as_deref() {
        let expiry = match parse_time("provider expires_at", expires_at) {
            Ok(value) => value,
            Err(error) => {
                return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
            }
        };
        if expiry <= now {
            return defer_with_status(
                provider,
                status,
                format!("{provider}: defer — bursar evidence expired at {expires_at}"),
            );
        }
    }

    let model = match optional_extra_string(status, "observation_model") {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    let expiry_basis = match optional_expiry_basis(status) {
        Ok(value) => value,
        Err(error) => {
            return decision_with_status(
                provider,
                status,
                model,
                None,
                BudgetAction::Defer,
                format!("{provider}: defer — {error}"),
            );
        }
    };

    let max_window_percent = match validate_window_percents(status) {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    if status.availability == Availability::Healthy {
        let Some(max_window_percent) = max_window_percent else {
            return decision_with_status(
                provider,
                status,
                model,
                expiry_basis,
                BudgetAction::SpendCautiously,
                format!(
                    "{provider}: spend-cautiously — bursar healthy status has no percent windows"
                ),
            );
        };
        if max_window_percent >= NEAR_EXHAUSTED_PERCENT {
            return decision_with_status(
                provider,
                status,
                model,
                expiry_basis,
                BudgetAction::Defer,
                format!(
                    "{provider}: defer — bursar window utilization {max_window_percent:.1}% is >= {NEAR_EXHAUSTED_PERCENT:.1}%"
                ),
            );
        }
    }

    let (action, label) = match status.availability {
        Availability::Healthy => (BudgetAction::Proceed, "proceed"),
        Availability::Caution => (BudgetAction::SpendCautiously, "spend-cautiously"),
        Availability::Exhausted | Availability::Unknown => (BudgetAction::Defer, "defer"),
    };
    decision_with_status(
        provider,
        status,
        model,
        expiry_basis,
        action,
        format!(
            "{provider}: {label} — bursar availability {}{}",
            status.availability,
            reason_suffix(status.reason.as_deref())
        ),
    )
}

fn validate_window_percents(status: &ProviderStatus) -> std::result::Result<Option<f64>, String> {
    status
        .windows
        .iter()
        .try_fold(None::<f64>, |max_percent, window| {
            let percent = window
                .percent
                .ok_or_else(|| format!("bursar window {} has no percent", window.label))?;
            if !percent.is_finite()
                || !(0.0..=100.0).contains(&percent)
                || (percent > 0.0 && percent <= 1.0)
            {
                return Err(format!(
                    "bursar window {} has invalid percent {percent:?}; expected 0 or >1..=100",
                    window.label
                ));
            }
            Ok(Some(
                max_percent.map_or(percent, |current| current.max(percent)),
            ))
        })
}

fn parse_time(label: &str, value: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| format!("malformed {label} {value:?}: {error}"))
}

fn optional_extra_string(
    status: &ProviderStatus,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match status.extra.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("malformed bursar extra.{key}")),
    }
}

fn optional_expiry_basis(status: &ProviderStatus) -> std::result::Result<Option<String>, String> {
    let value = optional_extra_string(status, "observation_expiry_basis")?;
    match value.as_deref() {
        None | Some("provider-reset" | "local-cooldown" | "human-override") => Ok(value),
        Some(other) => Err(format!(
            "unsupported bursar extra.observation_expiry_basis {other:?}"
        )),
    }
}

fn decision(provider: &str, action: BudgetAction, summary: String) -> BudgetDecision {
    BudgetDecision {
        provider: provider.to_string(),
        model: None,
        availability: None,
        source: None,
        checked_at: None,
        data_as_of: None,
        expires_at: None,
        expiry_basis: None,
        action,
        summary,
    }
}

fn defer_with_status(provider: &str, status: &ProviderStatus, summary: String) -> BudgetDecision {
    decision_with_status(provider, status, None, None, BudgetAction::Defer, summary)
}

fn decision_with_status(
    provider: &str,
    status: &ProviderStatus,
    model: Option<String>,
    expiry_basis: Option<String>,
    action: BudgetAction,
    summary: String,
) -> BudgetDecision {
    BudgetDecision {
        provider: provider.to_string(),
        model,
        availability: Some(status.availability),
        source: Some(status.source.clone()),
        checked_at: Some(status.checked_at.clone()),
        data_as_of: status.data_as_of.clone(),
        expires_at: status.expires_at.clone(),
        expiry_basis,
        action,
        summary,
    }
}

fn reason_suffix(reason: Option<&str>) -> String {
    reason.map_or_else(String::new, |reason| format!(" ({reason})"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone)]
    pub(crate) struct FakeBursarClient {
        result: Result<StatusReport>,
        observe_result: Result<()>,
        observations: Rc<RefCell<Vec<ObservationRequest>>>,
    }

    impl FakeBursarClient {
        fn from_result(result: Result<StatusReport>) -> Self {
            Self {
                result,
                observe_result: Ok(()),
                observations: Rc::new(RefCell::new(Vec::new())),
            }
        }

        pub(crate) fn unavailable() -> Self {
            Self::from_result(Err(BursarError::unavailable("bursar unavailable on PATH")))
        }

        pub(crate) fn with_provider_availability(
            provider: &str,
            availability: Availability,
        ) -> Self {
            Self::with_provider_availabilities(&[(provider, availability)])
        }

        pub(crate) fn with_provider_availabilities(
            availability_by_provider: &[(&str, Availability)],
        ) -> Self {
            let checked_at = Utc::now().to_rfc3339();
            let mut providers: BTreeMap<String, ProviderStatus> = PROVIDERS
                .into_iter()
                .map(|name| {
                    (
                        name.to_string(),
                        ProviderStatus {
                            availability: Availability::Unknown,
                            source: "test".to_string(),
                            checked_at: checked_at.clone(),
                            data_as_of: None,
                            expires_at: None,
                            windows: Vec::new(),
                            reason: Some("test status".to_string()),
                            extra: Map::new(),
                        },
                    )
                })
                .collect();
            for (provider, availability) in availability_by_provider {
                if let Some(status) = providers.get_mut(canonical_provider_key(provider)) {
                    status.availability = *availability;
                    status.reason =
                        (*availability != Availability::Healthy).then(|| "test status".to_string());
                    if *availability == Availability::Healthy {
                        status.windows = vec![Window {
                            label: "primary".to_string(),
                            percent: Some(42.0),
                            reset_at: Some("2100-01-01T00:00:00Z".to_string()),
                        }];
                    }
                }
            }
            Self::from_result(Ok(StatusReport {
                schema: SCHEMA.to_string(),
                checked_at,
                providers,
            }))
        }

        pub(crate) fn without_provider() -> Self {
            Self::from_result(Ok(StatusReport {
                schema: SCHEMA.to_string(),
                checked_at: Utc::now().to_rfc3339(),
                providers: PROVIDERS
                    .into_iter()
                    .map(|provider| {
                        (
                            provider.to_string(),
                            ProviderStatus {
                                availability: Availability::Unknown,
                                source: "test".to_string(),
                                checked_at: Utc::now().to_rfc3339(),
                                data_as_of: None,
                                expires_at: None,
                                windows: Vec::new(),
                                reason: Some("test status".to_string()),
                                extra: Map::new(),
                            },
                        )
                    })
                    .collect(),
            }))
        }

        pub(crate) fn with_observe_failure(mut self) -> Self {
            self.observe_result = Err(BursarError::command("fixture observe failure"));
            self
        }

        pub(crate) fn observations(&self) -> Vec<ObservationRequest> {
            self.observations.borrow().clone()
        }
    }

    impl BursarClient for FakeBursarClient {
        fn status(&self) -> Result<StatusReport> {
            self.result.clone()
        }

        fn observe(&self, request: &ObservationRequest) -> Result<()> {
            self.observations.borrow_mut().push(request.clone());
            self.observe_result.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::cell::Cell;
    use test_support::FakeBursarClient;

    #[derive(Clone)]
    struct FakeClient {
        result: Result<StatusReport>,
    }

    impl BursarClient for FakeClient {
        fn status(&self) -> Result<StatusReport> {
            self.result.clone()
        }
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn client_from_json(json: &str) -> FakeClient {
        FakeClient {
            result: serde_json::from_str(json)
                .map_err(|error| BursarError::json(error.to_string())),
        }
    }

    fn provider_roster_snapshot_fixture(
        provider_enabled: bool,
        provider_state: &str,
        availability: &str,
    ) -> (std::path::PathBuf, String) {
        let path = std::env::temp_dir().join(format!(
            "conductor-bursar-roster-{provider_enabled}-{provider_state}-{availability}.toml"
        ));
        std::fs::write(&path, "fixture roster\n").expect("write fixture roster");
        let bytes = std::fs::read(&path).expect("read fixture roster");
        let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
        let json = format!(
            r#"{{
  "schema": "bursar/roster@1",
  "generated_at": "2026-07-17T12:00:00Z",
  "artifact": {{"path": "{}", "sha256": "{}"}},
  "providers": [{{
    "provider_id": "anthropic",
    "availability_key": "anthropic",
    "enabled": {provider_enabled},
    "state": "{provider_state}",
    "availability": "{availability}",
    "checked_at": "2026-07-17T12:00:00Z",
    "data_as_of": "2026-07-17T11:59:00Z",
    "expires_at": "2026-07-17T14:00:00Z",
    "reason": "bounded manual allow",
    "eligible": true,
    "ineligibility_reason": null
  }}],
  "profiles": [{{
    "profile_id": "anthropic--claude-code--claude-opus-4-8--none",
    "provider_id": "anthropic",
    "model": "claude-opus-4-8",
    "harness": "claude-code",
    "dispatch_id": "claude-opus-4-8",
    "reasoning_effort": null,
    "tier": "lead",
    "ceiling": "XL",
    "efficiency": "heavy",
    "cost": 1.0,
    "data_policy": "standard",
    "enabled": true,
    "state": "healthy",
    "eligible": true,
    "ineligibility_reason": null
  }}]
}}"#,
            path.display(),
            sha256
        );
        (path, json)
    }

    #[test]
    fn roster_snapshot_preserves_profile_dispatch_identity() {
        let path = std::env::temp_dir().join("conductor-bursar-roster-snapshot.toml");
        std::fs::write(&path, "fixture roster\n").expect("write fixture roster");
        let bytes = std::fs::read(&path).expect("read fixture roster");
        let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
        let json = format!(
            r#"{{
  "schema": "bursar/roster@1",
  "generated_at": "2026-07-16T12:00:00Z",
  "artifact": {{"path": "{}", "sha256": "{}"}},
  "providers": [{{
    "provider_id": "openai-codex",
    "availability_key": "codex",
    "enabled": true,
    "state": "healthy",
    "availability": "healthy",
    "checked_at": "2026-07-16T12:00:00Z",
    "data_as_of": null,
    "expires_at": "2026-07-16T13:00:00Z",
    "reason": null,
    "eligible": true,
    "ineligibility_reason": null
  }}],
  "profiles": [{{
    "profile_id": "openai-codex--codex--gpt-5.6-luna--high",
    "provider_id": "openai-codex",
    "model": "gpt-5.6-luna",
    "harness": "codex",
    "dispatch_id": "gpt-5.6-luna",
    "reasoning_effort": "high",
    "tier": "senior",
    "ceiling": "L",
    "efficiency": "std",
    "cost": 1.0,
    "data_policy": "standard",
    "enabled": true,
    "state": "healthy",
    "eligible": true,
    "ineligibility_reason": null
  }}]
}}"#,
            path.display(),
            sha256
        );

        let snapshot = parse_roster_snapshot(json.as_bytes()).expect("valid roster snapshot");
        let roster = snapshot
            .roster_entries_with_fallbacks(&[])
            .expect("convert snapshot profiles");

        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name, "openai-codex--codex--gpt-5.6-luna--high");
        assert_eq!(roster[0].provider, "openai-codex");
        assert_eq!(roster[0].backend, crate::config::Backend::Codex);
        assert_eq!(roster[0].dispatch_id, "gpt-5.6-luna");
        assert_eq!(
            roster[0].reasoning_effort,
            Some(crate::config::ReasoningEffort::High)
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roster_snapshot_accepts_eligible_caution_provider() {
        let (path, json) = provider_roster_snapshot_fixture(true, "healthy", "caution");

        let snapshot = parse_roster_snapshot(json.as_bytes())
            .expect("eligible caution provider should be accepted");
        assert!(snapshot.providers[0].eligible);
        assert_eq!(snapshot.providers[0].availability, Availability::Caution);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roster_snapshot_rejects_inconsistent_eligible_provider_states() {
        for (enabled, state, availability) in [
            (false, "healthy", "healthy"),
            (true, "exhausted", "exhausted"),
            (true, "unknown", "unknown"),
            (true, "stale", "healthy"),
            (true, "manually-disabled", "healthy"),
        ] {
            let (path, json) = provider_roster_snapshot_fixture(enabled, state, availability);
            let error = parse_roster_snapshot(json.as_bytes())
                .expect_err("inconsistent eligible provider must fail closed");
            assert_eq!(
                error.to_string(),
                "eligible bursar roster provider is disabled or unavailable"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn provider_snapshot_normalizes_deduplicates_and_reads_bursar_once() {
        struct CountingClient {
            report: StatusReport,
            calls: Cell<usize>,
        }

        impl BursarClient for CountingClient {
            fn status(&self) -> Result<StatusReport> {
                self.calls.set(self.calls.get() + 1);
                Ok(self.report.clone())
            }
        }

        let report = FakeBursarClient::with_provider_availabilities(&[
            ("codex", Availability::Healthy),
            ("anthropic", Availability::Caution),
        ])
        .status()
        .unwrap();
        let client = CountingClient {
            report,
            calls: Cell::new(0),
        };

        let snapshot =
            evaluate_provider_snapshot(&client, ["openai-codex", "codex", " Anthropic "], true);

        assert_eq!(client.calls.get(), 1);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot["codex"].action, BudgetAction::Proceed);
        assert_eq!(snapshot["anthropic"].action, BudgetAction::SpendCautiously);
    }

    #[test]
    fn budget_validation_clock_is_sampled_after_status_collection() {
        struct PostScanClient;

        impl BursarClient for PostScanClient {
            fn status(&self) -> Result<StatusReport> {
                std::thread::sleep(std::time::Duration::from_millis(2));
                FakeBursarClient::with_provider_availability("codex", Availability::Healthy)
                    .status()
            }
        }

        let decision = evaluate_budget(&PostScanClient, "codex", true);

        assert_eq!(decision.action, BudgetAction::Proceed);
        assert!(!decision.summary.contains("future"));
    }

    #[test]
    fn status_v2_fixture_maps_all_availability_values_and_evidence() {
        let client = client_from_json(include_str!("../tests/fixtures/bursar-status-v2.json"));
        let now = at("2026-07-13T10:03:00Z");

        let anthropic = evaluate_budget_at(&client, "anthropic", true, now);
        assert_eq!(anthropic.action, BudgetAction::SpendCautiously);
        assert_eq!(anthropic.expiry_basis.as_deref(), Some("human-override"));
        assert_eq!(anthropic.model.as_deref(), Some("claude-opus-4-8"));

        let codex = evaluate_budget_at(&client, "openai-codex", true, now);
        assert_eq!(codex.provider, "codex");
        assert_eq!(codex.action, BudgetAction::Proceed);

        let opencode = evaluate_budget_at(&client, "opencode-go", true, now);
        assert_eq!(opencode.action, BudgetAction::Defer);
        assert_eq!(opencode.expiry_basis.as_deref(), Some("local-cooldown"));

        let agy = evaluate_budget_at(&client, "agy", true, now);
        assert_eq!(agy.action, BudgetAction::Defer);
        assert_eq!(agy.availability, Some(Availability::Unknown));
    }

    #[test]
    fn status_v2_fail_closed_cases_defer() {
        let now = at("2026-07-13T10:03:00Z");
        for json in [
            r#"{"schema":"bursar/status@1","checked_at":"2026-07-13T10:02:00Z","providers":{}}"#,
            r#"{"schema":"bursar/status@2","checked_at":"not-time","providers":{}}"#,
            r#"{"schema":"bursar/status@2","checked_at":"2026-07-13T09:00:00Z","providers":{}}"#,
            r#"{"schema":"bursar/status@2","checked_at":"2026-07-13T11:00:00Z","providers":{}}"#,
        ] {
            assert_eq!(
                evaluate_budget_at(&client_from_json(json), "codex", true, now).action,
                BudgetAction::Defer
            );
        }

        assert_eq!(
            evaluate_budget_at(&FakeBursarClient::unavailable(), "codex", true, now).action,
            BudgetAction::Defer
        );
        assert_eq!(
            evaluate_budget_at(
                &FakeBursarClient::without_provider(),
                "missing",
                true,
                Utc::now(),
            )
            .action,
            BudgetAction::Defer
        );
    }

    #[test]
    fn status_v2_requires_complete_fixed_provider_contract() {
        let now = at("2026-07-13T10:03:00Z");
        for field in ["data_as_of", "expires_at", "windows", "reason", "extra"] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["codex"]
                .as_object_mut()
                .expect("provider object")
                .remove(field);
            assert_eq!(
                evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now,)
                    .action,
                BudgetAction::Defer,
                "missing {field}"
            );
        }

        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        let unsupported = value["providers"]["agy"].take();
        let providers = value["providers"]
            .as_object_mut()
            .expect("providers object");
        providers.remove("agy");
        providers.insert("ollama-cloud".to_string(), unsupported);
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "ollama-cloud",
            true,
            now,
        );
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("baseline"));
    }

    #[test]
    fn status_v2_accepts_superset_with_new_providers() {
        // Regression for cycle-20260716-204555: Bursar commit e588018 extended
        // status@2 to cover every roster provider, but Conductor's length check
        // still required the legacy four. Every cycle candidate deferred as
        // "malformed bursar/status@2 provider set" and the fleet stopped.
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        let now = at("2026-07-13T10:03:00Z");
        for (name, availability) in [
            ("ollama-cloud", "healthy"),
            ("google-ai-studio", "caution"),
            ("neuralwatt", "exhausted"),
        ] {
            let mut provider = value["providers"]["codex"].clone();
            provider["availability"] = Value::String(availability.to_string());
            provider["reason"] = Value::String("test".to_string());
            value["providers"][name] = provider;
        }

        // Baseline providers keep their normal decisions on a superset report.
        let codex = evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(codex.action, BudgetAction::Proceed);

        // Added providers get their own Healthy/Caution/Exhausted decision.
        let ollama = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "ollama-cloud",
            true,
            now,
        );
        assert_eq!(ollama.action, BudgetAction::Proceed);
        assert_eq!(ollama.availability, Some(Availability::Healthy));

        let google = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "google-ai-studio",
            true,
            now,
        );
        assert_eq!(google.action, BudgetAction::SpendCautiously);
        assert_eq!(google.availability, Some(Availability::Caution));

        let neuralwatt = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "neuralwatt",
            true,
            now,
        );
        assert_eq!(neuralwatt.action, BudgetAction::Defer);
        assert_eq!(neuralwatt.availability, Some(Availability::Exhausted));

        // Requested provider absent from a superset still defers.
        let missing = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "missing-provider",
            true,
            now,
        );
        assert_eq!(missing.action, BudgetAction::Defer);
        assert!(missing.summary.contains("absent"));
    }

    #[test]
    fn status_v2_superset_missing_baseline_defers() {
        // Forward-compatible provider set must still require the legacy four.
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        let now = at("2026-07-13T10:03:00Z");
        let providers = value["providers"]
            .as_object_mut()
            .expect("providers object");
        let anthropic = providers.remove("anthropic").expect("anthropic present");
        providers.insert("ollama-cloud".to_string(), anthropic);
        providers.insert(
            "google-ai-studio".to_string(),
            providers.get("codex").expect("codex present").clone(),
        );
        providers.insert(
            "neuralwatt".to_string(),
            providers
                .get("opencode-go")
                .expect("opencode-go present")
                .clone(),
        );

        let decision =
            evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("baseline"));
    }

    #[test]
    fn status_v2_rejects_even_near_future_checked_at() {
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        value["checked_at"] = Value::String("2026-07-13T10:03:01Z".to_string());
        for provider in PROVIDERS {
            value["providers"][provider]["checked_at"] =
                Value::String("2026-07-13T10:03:01Z".to_string());
        }
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "codex",
            true,
            at("2026-07-13T10:03:00Z"),
        );
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("future"));
    }

    #[test]
    fn provider_timestamps_and_expiry_fail_closed() {
        let now = at("2026-07-13T10:03:00Z");
        for (field, value) in [
            ("checked_at", "bad"),
            ("checked_at", "2026-07-13T10:01:00Z"),
            ("data_as_of", "2026-07-13T10:04:00Z"),
            ("data_as_of", "bad"),
            ("expires_at", "2026-07-13T10:03:00Z"),
            ("expires_at", "bad"),
        ] {
            let mut value_json: Value =
                serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                    .expect("fixture JSON");
            value_json["providers"]["codex"][field] = Value::String(value.to_string());
            let client = client_from_json(&value_json.to_string());
            assert_eq!(
                evaluate_budget_at(&client, "codex", true, now).action,
                BudgetAction::Defer,
                "{field}={value}"
            );
        }
    }

    #[test]
    fn malformed_observation_metadata_fails_closed() {
        let now = at("2026-07-13T10:03:00Z");
        for bad in [Value::Bool(true), Value::String("invented".to_string())] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["anthropic"]["extra"]["observation_expiry_basis"] = bad;
            let decision = evaluate_budget_at(
                &client_from_json(&value.to_string()),
                "anthropic",
                true,
                now,
            );
            assert_eq!(decision.action, BudgetAction::Defer);
            assert!(decision.summary.contains("observation_expiry_basis"));
        }
    }

    #[test]
    fn healthy_provider_requires_bounded_non_fractional_window_percent() {
        let now = at("2026-07-13T10:03:00Z");
        for percent in [
            Value::Null,
            Value::from(0.42),
            Value::from(1.0),
            Value::from(-1.0),
            Value::from(100.1),
        ] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["codex"]["windows"] = Value::Array(vec![serde_json::json!({
                "label": "primary",
                "percent": percent,
                "reset_at": "2100-01-01T00:00:00Z"
            })]);
            let decision =
                evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
            assert_eq!(decision.action, BudgetAction::Defer);
            assert!(decision.summary.contains("percent"));
        }

        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        value["providers"]["codex"]["windows"] = Value::Array(Vec::new());
        let decision =
            evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(decision.action, BudgetAction::SpendCautiously);
        assert!(decision.summary.contains("no percent windows"));
    }

    #[test]
    fn healthy_provider_defers_at_near_exhausted_window_percent() {
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/bursar-status-v2.json"))
                .expect("fixture JSON");
        value["providers"]["codex"]["windows"][0]["percent"] = Value::from(90.0);
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "codex",
            true,
            at("2026-07-13T10:03:00Z"),
        );
        assert_eq!(decision.action, BudgetAction::Defer);
    }

    #[test]
    fn disabled_mode_is_the_only_static_caps_override() {
        let decision = evaluate_budget(&FakeBursarClient::unavailable(), "openai-codex", false);
        assert_eq!(decision.provider, "codex");
        assert_eq!(decision.action, BudgetAction::StaticCaps);
        assert!(decision.summary.contains("budgets.use_bursar is false"));
    }

    #[test]
    fn observation_request_builds_exact_sanitized_bursar_argv() {
        let request = ObservationRequest::runtime_limit(
            "openai-codex",
            Some("gpt-5.6-terra".to_string()),
            "2026-07-13T10:18:00Z",
            ObservationExpiryBasis::LocalCooldown,
            RuntimeLimitReason::Http429,
        );
        assert_eq!(
            observation_args(&request),
            [
                "observe",
                "--provider",
                "codex",
                "--availability",
                "exhausted",
                "--expires-at",
                "2026-07-13T10:18:00Z",
                "--expiry-basis",
                "local-cooldown",
                "--source",
                "conductor-runtime",
                "--reason",
                "runtime HTTP 429",
                "--model",
                "gpt-5.6-terra",
            ]
        );
    }

    #[test]
    fn observation_reason_and_basis_are_closed_enums() {
        assert_eq!(
            ObservationExpiryBasis::ProviderReset.label(),
            "provider-reset"
        );
        assert_eq!(
            RuntimeLimitReason::QuotaExceeded.label(),
            "runtime quota exceeded"
        );
        assert_eq!(RuntimeLimitReason::RateLimit.label(), "runtime rate limit");
    }
}
