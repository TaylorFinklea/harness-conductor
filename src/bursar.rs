//! Bursar `status --json` client and budget decision helpers.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::process::{Command, Stdio};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

const SCHEMA: &str = "bursar/status@2";
const MAX_STATUS_AGE_MINS: i64 = 5;
const PROVIDERS: [&str; 4] = ["anthropic", "codex", "opencode-go", "agy"];

pub(crate) type Result<T> = std::result::Result<T, BursarError>;

pub(crate) trait BursarClient {
    fn status(&self) -> Result<StatusReport>;

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
