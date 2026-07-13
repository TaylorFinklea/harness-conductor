//! Bursar `status --json` client and budget decision helpers.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{Map, Value};

const SCHEMA: &str = "bursar/status@1";
const NEAR_EXHAUSTED_PERCENT: f64 = 90.0;

pub(crate) type Result<T> = std::result::Result<T, BursarError>;

pub(crate) trait BursarClient {
    fn status(&self) -> Result<StatusReport>;
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
#[serde(rename_all = "lowercase")]
pub(crate) enum ProviderState {
    Ok,
    Unknown,
    Error,
}

impl fmt::Display for ProviderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Unknown => f.write_str("unknown"),
            Self::Error => f.write_str("error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Window {
    pub(crate) label: String,
    pub(crate) percent: Option<f64>,
    pub(crate) reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ProviderStatus {
    pub(crate) status: ProviderState,
    pub(crate) source: String,
    pub(crate) checked_at: String,
    pub(crate) data_as_of: Option<String>,
    pub(crate) windows: Vec<Window>,
    pub(crate) reason: Option<String>,
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct StatusReport {
    pub(crate) schema: String,
    pub(crate) checked_at: String,
    pub(crate) providers: BTreeMap<String, ProviderStatus>,
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
    pub(crate) action: BudgetAction,
    pub(crate) summary: String,
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
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => BursarError::unavailable("bursar unavailable on PATH"),
                _ => BursarError::command(format!("failed to spawn bursar status --json: {e}")),
            })?;

        if !output.status.success() {
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
            return Err(BursarError::command(format!(
                "bursar status --json exited {status}: {detail}"
            )));
        }

        serde_json::from_slice(&output.stdout)
            .map_err(|e| BursarError::json(format!("failed to parse bursar status --json: {e}")))
    }
}

pub(crate) fn evaluate_budget<C: BursarClient + ?Sized>(
    client: &C,
    provider: &str,
    use_bursar: bool,
) -> BudgetDecision {
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
                BudgetAction::SpendCautiously,
                format!("{provider}: spend-cautiously — bursar unavailable ({error})"),
            );
        }
        Err(error) => {
            return decision(
                provider,
                BudgetAction::SpendCautiously,
                format!("{provider}: spend-cautiously — bursar status error: {error}"),
            );
        }
    };

    if report.schema != SCHEMA {
        return decision(
            provider,
            BudgetAction::SpendCautiously,
            format!(
                "{provider}: spend-cautiously — unsupported bursar schema {}",
                report.schema
            ),
        );
    }

    let Some(status) = report.providers.get(provider) else {
        return decision(
            provider,
            BudgetAction::SpendCautiously,
            format!("{provider}: spend-cautiously — provider absent from bursar/status@1"),
        );
    };

    match status.status {
        ProviderState::Unknown | ProviderState::Error => decision(
            provider,
            BudgetAction::SpendCautiously,
            format!(
                "{provider}: spend-cautiously — bursar status {}{}",
                status.status,
                reason_suffix(status.reason.as_deref())
            ),
        ),
        ProviderState::Ok => evaluate_ok_provider(provider, status),
    }
}

fn evaluate_ok_provider(provider: &str, status: &ProviderStatus) -> BudgetDecision {
    if let Some(window) = status
        .windows
        .iter()
        .filter_map(|window| window.percent.map(|percent| (window, percent)))
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
    {
        let (window, percent) = window;
        if percent >= NEAR_EXHAUSTED_PERCENT {
            return decision(
                provider,
                BudgetAction::Defer,
                format!(
                    "{provider}: defer — bursar window {} at {percent:.1}% (>= {NEAR_EXHAUSTED_PERCENT:.1}%)",
                    window.label
                ),
            );
        }
        return decision(
            provider,
            BudgetAction::Proceed,
            format!(
                "{provider}: proceed — bursar window {} at {percent:.1}% (< {NEAR_EXHAUSTED_PERCENT:.1}%)",
                window.label
            ),
        );
    }

    decision(
        provider,
        BudgetAction::SpendCautiously,
        format!("{provider}: spend-cautiously — bursar status ok but no percent windows"),
    )
}

fn decision(provider: &str, action: BudgetAction, summary: String) -> BudgetDecision {
    BudgetDecision {
        provider: provider.to_string(),
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

    #[derive(Debug, Clone)]
    pub(crate) struct FakeBursarClient {
        result: Result<StatusReport>,
    }

    impl FakeBursarClient {
        pub(crate) fn unavailable() -> Self {
            Self {
                result: Err(BursarError::unavailable("bursar unavailable on PATH")),
            }
        }

        pub(crate) fn with_provider_status(
            provider: &str,
            state: ProviderState,
            percent: Option<f64>,
        ) -> Self {
            let mut providers = BTreeMap::new();
            let windows = percent
                .map(|percent| {
                    vec![Window {
                        label: "5h".to_string(),
                        percent: Some(percent),
                        reset_at: Some("2026-07-07T12:00:00Z".to_string()),
                    }]
                })
                .unwrap_or_default();
            providers.insert(
                provider.to_string(),
                ProviderStatus {
                    status: state,
                    source: "test".to_string(),
                    checked_at: "2026-07-07T00:00:00Z".to_string(),
                    data_as_of: None,
                    windows,
                    reason: (state != ProviderState::Ok).then(|| "test status".to_string()),
                    extra: Map::new(),
                },
            );
            Self {
                result: Ok(StatusReport {
                    schema: SCHEMA.to_string(),
                    checked_at: "2026-07-07T00:00:00Z".to_string(),
                    providers,
                }),
            }
        }
    }

    impl BursarClient for FakeBursarClient {
        fn status(&self) -> Result<StatusReport> {
            self.result.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::FakeBursarClient;

    #[test]
    fn evaluate_budget_unavailable_bursar_spends_cautiously_not_static_caps() {
        let client = FakeBursarClient::unavailable();

        let decision = evaluate_budget(&client, "opencode-go", true);

        // A missing bursar binary is not less uncertain than a bursar that ran
        // and reported "unknown" — it must inherit the same cautious floor,
        // never the permissive static-caps path (that path is reserved for
        // the explicit `budgets.use_bursar = false` override).
        assert_ne!(decision.action, BudgetAction::StaticCaps);
        assert_eq!(decision.action, BudgetAction::SpendCautiously);
        assert!(decision.summary.contains("bursar unavailable"));
    }
}
