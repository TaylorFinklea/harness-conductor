//! Scorecard-vs-`conductor.toml` roster drift detector.
//!
//! Parses the "Live Roster" markdown table in `~/.claude/model-scorecard.md`
//! (columns: Model, Dispatch ID, Tier, Ceiling, Reliability) and diffs it
//! against `conductor.toml`'s roster. Reports missing models, extra models,
//! and tier/ceiling mismatches.
//!
//! WARN only — stdout report, exit 0 — unless the scorecard file is
//! unreadable or unparseable, in which case the caller should exit 1.
//! Never auto-edits either file.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;

use crate::config;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors from scorecard parsing or IO.
#[derive(Debug)]
pub(crate) struct RosterDriftError {
    message: String,
}

impl RosterDriftError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl fmt::Display for RosterDriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RosterDriftError {}

type Result<T> = std::result::Result<T, RosterDriftError>;

// ---------------------------------------------------------------------------
// Scorecard entry (parsed from markdown)
// ---------------------------------------------------------------------------

/// One row from the scorecard's Live Roster markdown table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScorecardEntry {
    /// Display name from the Model column.
    pub(crate) model: String,
    /// Dispatch ID from the Dispatch ID column.
    pub(crate) dispatch_id: String,
    /// Tier from the Tier column (e.g. "Lead", "Senior + Junior").
    pub(crate) tier: String,
    /// Ceiling from the Ceiling column (e.g. "XL (via decomposition)").
    pub(crate) ceiling: String,
}

// ---------------------------------------------------------------------------
// Drift report
// ---------------------------------------------------------------------------

/// One difference between the scorecard and the config roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriftItem {
    /// What kind of drift was detected.
    pub(crate) kind: DriftKind,
    /// The model name this drift relates to.
    pub(crate) model: String,
    /// Human-readable description of the drift.
    pub(crate) detail: String,
}

/// The kind of roster drift detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriftKind {
    /// Model is in the scorecard but not in `conductor.toml`.
    MissingFromConfig,
    /// Model is in `conductor.toml` but not in the scorecard.
    ExtraInConfig,
    /// Tier differs between scorecard and config.
    TierMismatch,
    /// Ceiling differs between scorecard and config.
    CeilingMismatch,
}

impl fmt::Display for DriftKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFromConfig => f.write_str("missing from config"),
            Self::ExtraInConfig => f.write_str("extra in config"),
            Self::TierMismatch => f.write_str("tier mismatch"),
            Self::CeilingMismatch => f.write_str("ceiling mismatch"),
        }
    }
}

/// The complete drift report: all differences found between the two sources.
#[derive(Debug, Clone, Default)]
pub(crate) struct DriftReport {
    /// Individual drift items.
    pub(crate) items: Vec<DriftItem>,
}

impl DriftReport {
    /// Whether any drift was detected.
    pub(crate) fn has_drift(&self) -> bool {
        !self.items.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Normalization helpers
// ---------------------------------------------------------------------------

/// Lowercased, trimmed model name for matching across sources.
fn normalize_model(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Strips the first parenthetical group and trims trailing whitespace.
fn strip_parenthetical(s: &str) -> &str {
    match s.find('(') {
        Some(pos) => s[..pos].trim_end(),
        None => s,
    }
}

/// Extracts the first whitespace-delimited word.
fn strip_first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or(s)
}

/// Uppercase first word of a tier string (e.g. "Lead (all but hardest) / Senior" → "LEAD").
fn normalize_tier(s: &str) -> String {
    strip_first_word(strip_parenthetical(s.trim())).to_uppercase()
}

/// Uppercase first word of a ceiling string (e.g. "XL (via decomposition)" → "XL").
fn normalize_ceiling(s: &str) -> String {
    strip_first_word(strip_parenthetical(s.trim())).to_uppercase()
}

/// Strips bold markers, inline code backticks, and parenthetical notes from a
/// markdown table cell value.
fn clean_cell(s: &str) -> String {
    let s = s.trim();
    let s = strip_parenthetical(s);
    let s = s.trim();
    let s = s.strip_prefix("**").unwrap_or(s);
    let s = s.strip_suffix("**").unwrap_or(s);
    let s = s.trim();
    let s = s.strip_prefix('`').unwrap_or(s);
    let s = s.strip_suffix('`').unwrap_or(s);
    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// Scorecard markdown parser
// ---------------------------------------------------------------------------

/// Parses the Live Roster markdown table from scorecard content.
///
/// Looks for a `## Live Roster` heading, then finds the first markdown table
/// whose header row contains a "Model" column. Tolerates bold markers
/// (`**...**`), inline code (`` `...` ``), and parenthetical notes inside cells.
///
/// # Errors
///
/// Returns an error if the Live Roster section is missing, the table header
/// cannot be found, or required columns (Model, Tier, Ceiling) are absent.
pub(crate) fn parse_scorecard(content: &str) -> Result<Vec<ScorecardEntry>> {
    let lines: Vec<&str> = content.lines().collect();

    let section_start = find_live_roster_section(&lines).ok_or_else(|| {
        RosterDriftError::new("could not find '## Live Roster' section in scorecard")
    })?;

    let (header_idx, col_map) =
        find_table_header(&lines, section_start).ok_or_else(|| {
            RosterDriftError::new(
                "could not find markdown table with a Model column after Live Roster heading",
            )
        })?;

    let model_col = *col_map
        .get("model")
        .ok_or_else(|| RosterDriftError::new("missing required column: Model"))?;
    let tier_col = *col_map
        .get("tier")
        .ok_or_else(|| RosterDriftError::new("missing required column: Tier"))?;
    let ceiling_col = *col_map
        .get("ceiling")
        .ok_or_else(|| RosterDriftError::new("missing required column: Ceiling"))?;
    let dispatch_col = col_map.get("dispatch id").copied();

    let data_start = skip_separator(&lines, header_idx + 1).ok_or_else(|| {
        RosterDriftError::new("expected separator row after table header")
    })?;

    let mut entries = Vec::new();
    for &line in &lines[data_start..] {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            break;
        }
        if !line.contains('|') {
            break;
        }
        let cells = split_table_row(line);
        let model = cells
            .get(model_col)
            .map(|c| clean_cell(c))
            .unwrap_or_default();
        if model.is_empty() {
            continue;
        }
        let dispatch_id = dispatch_col
            .and_then(|col| cells.get(col))
            .map(|c| clean_cell(c))
            .unwrap_or_default();
        let tier = cells
            .get(tier_col)
            .map(|c| clean_cell(c))
            .unwrap_or_default();
        let ceiling = cells
            .get(ceiling_col)
            .map(|c| clean_cell(c))
            .unwrap_or_default();
        entries.push(ScorecardEntry {
            model,
            dispatch_id,
            tier,
            ceiling,
        });
    }

    if entries.is_empty() {
        return Err(RosterDriftError::new(
            "Live Roster table contains no data rows",
        ));
    }

    Ok(entries)
}

fn find_live_roster_section(lines: &[&str]) -> Option<usize> {
    lines.iter().position(|l| {
        let t = l.trim().to_lowercase();
        t.starts_with("## live roster") || t == "## live roster"
    })
}

fn find_table_header(
    lines: &[&str],
    start: usize,
) -> Option<(usize, HashMap<String, usize>)> {
    for (i, &line) in lines.iter().enumerate().skip(start) {
        if !line.contains('|') {
            continue;
        }
        let cells = split_table_row(line);
        let has_model = cells
            .iter()
            .any(|c| clean_cell(c).eq_ignore_ascii_case("model"));
        if !has_model {
            continue;
        }
        if lines.get(i + 1).is_some_and(|l| is_separator_row(l)) {
            let col_map: HashMap<String, usize> = cells
                .iter()
                .enumerate()
                .map(|(idx, c)| (clean_cell(c).to_lowercase(), idx))
                .collect();
            return Some((i, col_map));
        }
    }
    None
}

fn is_separator_row(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
        && trimmed.contains('-')
}

fn skip_separator(lines: &[&str], idx: usize) -> Option<usize> {
    let line = lines.get(idx)?;
    if is_separator_row(line) {
        Some(idx + 1)
    } else {
        None
    }
}

fn split_table_row(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    let inner = match (trimmed.strip_prefix('|'), trimmed.strip_suffix('|')) {
        (Some(s), Some(_) | None) | (None, Some(s)) => s,
        (None, None) => trimmed,
    };
    inner.split('|').collect()
}

// ---------------------------------------------------------------------------
// Diff: scorecard vs config
// ---------------------------------------------------------------------------

/// Compares scorecard entries against config roster entries and produces a
/// drift report listing missing models, extra models, and tier/ceiling
/// mismatches.
pub(crate) fn diff(
    scorecard: &[ScorecardEntry],
    config_roster: &[config::RosterEntry],
) -> DriftReport {
    let mut report = DriftReport::default();

    let sc_by_name: HashMap<String, &ScorecardEntry> = scorecard
        .iter()
        .map(|e| (normalize_model(&e.model), e))
        .collect();

    let cfg_by_name: HashMap<String, &config::RosterEntry> = config_roster
        .iter()
        .map(|e| (normalize_model(&e.name), e))
        .collect();

    // Missing from config (in scorecard but not in config)
    for entry in scorecard {
        let key = normalize_model(&entry.model);
        if !cfg_by_name.contains_key(&key) {
            report.items.push(DriftItem {
                kind: DriftKind::MissingFromConfig,
                model: entry.model.clone(),
                detail: format!(
                    "in scorecard but not in conductor.toml (tier: {}, ceiling: {})",
                    entry.tier, entry.ceiling
                ),
            });
        }
    }

    // Extra in config (in config but not in scorecard)
    for entry in config_roster {
        let key = normalize_model(&entry.name);
        if !sc_by_name.contains_key(&key) {
            report.items.push(DriftItem {
                kind: DriftKind::ExtraInConfig,
                model: entry.name.clone(),
                detail: "in conductor.toml but not in scorecard".to_string(),
            });
        }
    }

    // Mismatches for models present in both
    for sc_entry in scorecard {
        let key = normalize_model(&sc_entry.model);
        if let Some(cfg_entry) = cfg_by_name.get(&key) {
            let sc_tier = normalize_tier(&sc_entry.tier);
            let cfg_tier = format!("{:?}", cfg_entry.tier).to_uppercase();
            if sc_tier != cfg_tier {
                report.items.push(DriftItem {
                    kind: DriftKind::TierMismatch,
                    model: sc_entry.model.clone(),
                    detail: format!(
                        "scorecard: {}, config: {:?}",
                        sc_entry.tier, cfg_entry.tier
                    ),
                });
            }

            let sc_ceiling = normalize_ceiling(&sc_entry.ceiling);
            let cfg_ceiling = format!("{:?}", cfg_entry.ceiling).to_uppercase();
            if sc_ceiling != cfg_ceiling {
                report.items.push(DriftItem {
                    kind: DriftKind::CeilingMismatch,
                    model: sc_entry.model.clone(),
                    detail: format!(
                        "scorecard: {}, config: {:?}",
                        sc_entry.ceiling, cfg_entry.ceiling
                    ),
                });
            }
        }
    }

    report
}

/// Prints the drift report to stdout.
pub(crate) fn print_report(report: &DriftReport) {
    if report.items.is_empty() {
        println!("roster drift: none — scorecard and conductor.toml agree");
        return;
    }
    println!(
        "roster drift: {} difference(s) detected",
        report.items.len()
    );
    println!();
    for item in &report.items {
        println!("  [{:>20}] {}", item.kind, item.model);
        println!("    {}", item.detail);
    }
    println!();
    println!("config is authoritative at dispatch time; review and reconcile manually.");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Ceiling, Efficiency, Backend, Tier};

    fn cfg_entry(name: &str, tier: Tier, ceiling: Ceiling) -> config::RosterEntry {
        config::RosterEntry {
            name: name.to_string(),
            tier,
            ceiling,
            efficiency: Efficiency::Std,
            backend: Backend::Pi,
            dispatch_id: format!("dispatch-{name}"),
            provider: String::new(),
            cost: config::Cost::Paid,
            fallback: Vec::new(),
        }
    }

    // --- parse_scorecard ---

    #[test]
    fn roster_drift_parse_basic_table() {
        let md = "\
## Live Roster

| Model | Dispatch ID | Tier (owns) | Ceiling | Reliability |
|---|---|---|---|---|
| **opus-4.8** | Claude main loop | **Lead** | XL | high |
| **sonnet-5** | Claude Task subagent | **Lead** | L | high |
";
        let entries = parse_scorecard(md).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].model, "opus-4.8");
        assert_eq!(entries[0].tier, "Lead");
        assert_eq!(entries[0].ceiling, "XL");
        assert_eq!(entries[1].model, "sonnet-5");
        assert_eq!(entries[1].tier, "Lead");
        assert_eq!(entries[1].ceiling, "L");
    }

    #[test]
    fn roster_drift_parse_strips_parenthetical_notes() {
        let md = "\
## Live Roster

| Model | Dispatch ID | Tier | Ceiling | Reliability |
|---|---|---|---|---|
| **sonnet-5** | `Claude Task` | **Lead (all but hardest) / Senior** | XL (via decomposition) | high (7+ clean runs) |
";
        let entries = parse_scorecard(md).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tier, "Lead");
        assert_eq!(entries[0].ceiling, "XL");
    }

    #[test]
    fn roster_drift_parse_strips_inline_code_backticks() {
        let md = "\
## Live Roster

| Model | Dispatch ID | Tier | Ceiling | Reliability |
|---|---|---|---|---|
| **glm-5.2** | `opencode-go/glm-5.2` (pi) | **Senior** | M | good |
";
        let entries = parse_scorecard(md).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].dispatch_id, "opencode-go/glm-5.2");
    }

    #[test]
    fn roster_drift_parse_error_missing_section() {
        let md = "# Some other document\n\nNo live roster here.\n";
        assert!(parse_scorecard(md).is_err());
    }

    #[test]
    fn roster_drift_parse_error_no_table() {
        let md = "\
## Live Roster

Just some prose, no table at all.
";
        assert!(parse_scorecard(md).is_err());
    }

    #[test]
    fn roster_drift_parse_error_empty_table() {
        let md = "\
## Live Roster

| Model | Dispatch ID | Tier | Ceiling | Reliability |
|---|---|---|---|---|
";
        assert!(parse_scorecard(md).is_err());
    }

    #[test]
    fn roster_drift_parse_error_missing_tier_column() {
        let md = "\
## Live Roster

| Model | Dispatch ID | Ceiling | Reliability |
|---|---|---|---|
| **x** | y | M | high |
";
        assert!(parse_scorecard(md).is_err());
    }

    // --- diff: agreement ---

    #[test]
    fn roster_drift_diff_agreement() {
        let sc = vec![
            ScorecardEntry {
                model: "opus-4.8".to_string(),
                dispatch_id: "claude-opus-4-8".to_string(),
                tier: "Lead".to_string(),
                ceiling: "XL".to_string(),
            },
            ScorecardEntry {
                model: "sonnet-5".to_string(),
                dispatch_id: "claude-sonnet-5".to_string(),
                tier: "Lead".to_string(),
                ceiling: "L".to_string(),
            },
        ];
        let cfg = vec![
            cfg_entry("opus-4.8", Tier::Lead, Ceiling::Xl),
            cfg_entry("sonnet-5", Tier::Lead, Ceiling::L),
        ];
        let report = diff(&sc, &cfg);
        assert!(!report.has_drift());
    }

    #[test]
    fn roster_drift_diff_agreement_case_insensitive_model_name() {
        let sc = vec![ScorecardEntry {
            model: "Sonnet-5".to_string(),
            dispatch_id: String::new(),
            tier: "Lead".to_string(),
            ceiling: "L".to_string(),
        }];
        let cfg = vec![cfg_entry("sonnet-5", Tier::Lead, Ceiling::L)];
        let report = diff(&sc, &cfg);
        assert!(!report.has_drift());
    }

    // --- diff: missing from config ---

    #[test]
    fn roster_drift_diff_missing_from_config() {
        let sc = vec![
            ScorecardEntry {
                model: "opus-4.8".to_string(),
                dispatch_id: String::new(),
                tier: "Lead".to_string(),
                ceiling: "XL".to_string(),
            },
            ScorecardEntry {
                model: "new-model".to_string(),
                dispatch_id: String::new(),
                tier: "Senior".to_string(),
                ceiling: "M".to_string(),
            },
        ];
        let cfg = vec![cfg_entry("opus-4.8", Tier::Lead, Ceiling::Xl)];
        let report = diff(&sc, &cfg);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, DriftKind::MissingFromConfig);
        assert_eq!(report.items[0].model, "new-model");
    }

    // --- diff: extra in config ---

    #[test]
    fn roster_drift_diff_extra_in_config() {
        let sc = vec![ScorecardEntry {
            model: "opus-4.8".to_string(),
            dispatch_id: String::new(),
            tier: "Lead".to_string(),
            ceiling: "XL".to_string(),
        }];
        let cfg = vec![
            cfg_entry("opus-4.8", Tier::Lead, Ceiling::Xl),
            cfg_entry("old-model", Tier::Junior, Ceiling::S),
        ];
        let report = diff(&sc, &cfg);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, DriftKind::ExtraInConfig);
        assert_eq!(report.items[0].model, "old-model");
    }

    // --- diff: tier mismatch ---

    #[test]
    fn roster_drift_diff_tier_mismatch() {
        let sc = vec![ScorecardEntry {
            model: "sonnet-5".to_string(),
            dispatch_id: String::new(),
            tier: "Senior".to_string(),
            ceiling: "L".to_string(),
        }];
        let cfg = vec![cfg_entry("sonnet-5", Tier::Lead, Ceiling::L)];
        let report = diff(&sc, &cfg);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, DriftKind::TierMismatch);
    }

    // --- diff: ceiling mismatch ---

    #[test]
    fn roster_drift_diff_ceiling_mismatch() {
        let sc = vec![ScorecardEntry {
            model: "sonnet-5".to_string(),
            dispatch_id: String::new(),
            tier: "Lead".to_string(),
            ceiling: "XL".to_string(),
        }];
        let cfg = vec![cfg_entry("sonnet-5", Tier::Lead, Ceiling::L)];
        let report = diff(&sc, &cfg);
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].kind, DriftKind::CeilingMismatch);
    }

    // --- diff: multiple drift types ---

    #[test]
    fn roster_drift_diff_all_drift_types_at_once() {
        let sc = vec![
            ScorecardEntry {
                model: "opus-4.8".to_string(),
                dispatch_id: String::new(),
                tier: "Lead".to_string(),
                ceiling: "XL".to_string(),
            },
            ScorecardEntry {
                model: "new-guy".to_string(),
                dispatch_id: String::new(),
                tier: "Junior".to_string(),
                ceiling: "S".to_string(),
            },
        ];
        let cfg = vec![
            cfg_entry("opus-4.8", Tier::Senior, Ceiling::M),
            cfg_entry("old-guy", Tier::Lead, Ceiling::Xl),
        ];
        let report = diff(&sc, &cfg);
        let kinds: Vec<DriftKind> = report.items.iter().map(|i| i.kind.clone()).collect();
        assert!(kinds.contains(&DriftKind::MissingFromConfig));
        assert!(kinds.contains(&DriftKind::ExtraInConfig));
        assert!(kinds.contains(&DriftKind::TierMismatch));
        assert!(kinds.contains(&DriftKind::CeilingMismatch));
    }

    // --- normalization ---

    #[test]
    fn roster_drift_normalize_tier_strips_parenthetical_and_extras() {
        assert_eq!(normalize_tier("Lead (all but hardest) / Senior"), "LEAD");
        assert_eq!(normalize_tier("Senior + Junior"), "SENIOR");
        assert_eq!(normalize_tier("Junior"), "JUNIOR");
    }

    #[test]
    fn roster_drift_normalize_ceiling_strips_parenthetical() {
        assert_eq!(normalize_ceiling("XL (via decomposition)"), "XL");
        assert_eq!(normalize_ceiling("M"), "M");
        assert_eq!(normalize_ceiling("S"), "S");
        assert_eq!(normalize_ceiling("L"), "L");
    }

    #[test]
    fn roster_drift_clean_cell_handles_bold_and_code() {
        assert_eq!(clean_cell("**opus-4.8**"), "opus-4.8");
        assert_eq!(clean_cell("`opencode-go/glm-5.2`"), "opencode-go/glm-5.2");
        assert_eq!(clean_cell("**Lead** (all but hardest)"), "Lead");
        assert_eq!(
            clean_cell("high (7+ clean runs)"),
            "high"
        );
    }

    // Fixture-based tests
    fn load_fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}"))
            .unwrap_or_else(|e| panic!("Failed to read fixture {name}: {e}"))
    }

    #[test]
    fn roster_drift_fixture_agreement() {
        let scorecard = load_fixture("scorecard-agreement.md");
        let entries = parse_scorecard(&scorecard).expect("Failed to parse fixture");
        assert_eq!(entries.len(), 7);
    }

    #[test]
    fn roster_drift_fixture_missing_from_config() {
        let scorecard = load_fixture("scorecard-missing-from-config.md");
        let entries = parse_scorecard(&scorecard).expect("Failed to parse fixture");
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[7].model, "new-model");
    }

    #[test]
    fn roster_drift_fixture_extra_in_config() {
        let scorecard = load_fixture("scorecard-extra-in-config.md");
        let entries = parse_scorecard(&scorecard).expect("Failed to parse fixture");
        assert_eq!(entries.len(), 6);
    }

    #[test]
    fn roster_drift_fixture_tier_mismatch() {
        let scorecard = load_fixture("scorecard-tier-mismatch.md");
        let entries = parse_scorecard(&scorecard).expect("Failed to parse fixture");
        assert_eq!(entries.len(), 7);
        let sonnet = entries.iter().find(|e| e.model == "sonnet-5").unwrap();
        assert_eq!(sonnet.tier, "Senior");
    }

    #[test]
    fn roster_drift_fixture_ceiling_mismatch() {
        let scorecard = load_fixture("scorecard-ceiling-mismatch.md");
        let entries = parse_scorecard(&scorecard).expect("Failed to parse fixture");
        assert_eq!(entries.len(), 7);
        let sonnet = entries.iter().find(|e| e.model == "sonnet-5").unwrap();
        assert_eq!(sonnet.ceiling, "XL");
    }

    #[test]
    fn roster_drift_fixture_unparseable() {
        let scorecard = load_fixture("scorecard-unparseable.md");
        let result = parse_scorecard(&scorecard);
        assert!(result.is_err(), "Expected parse error for unparseable fixture");
    }

    fn load_conductor_config() -> config::Config {
        config::parse_str(include_str!("../conductor.toml")).expect("conductor.toml must parse")
    }

    #[test]
    fn roster_drift_diff_fixture_agreement_against_real_config() {
        let entries = parse_scorecard(&load_fixture("scorecard-agreement.md")).unwrap();
        let cfg = load_conductor_config();
        let report = diff(&entries, &cfg.roster);
        assert!(!report.has_drift(), "expected no drift, got: {:?}", report.items);
    }

    #[test]
    fn roster_drift_diff_fixture_missing_from_config_against_real_config() {
        let entries = parse_scorecard(&load_fixture("scorecard-missing-from-config.md")).unwrap();
        let cfg = load_conductor_config();
        let report = diff(&entries, &cfg.roster);
        let missing: Vec<_> = report.items.iter().filter(|i| i.kind == DriftKind::MissingFromConfig).collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].model, "new-model");
    }

    #[test]
    fn roster_drift_diff_fixture_extra_in_config_against_real_config() {
        let entries = parse_scorecard(&load_fixture("scorecard-extra-in-config.md")).unwrap();
        let cfg = load_conductor_config();
        let report = diff(&entries, &cfg.roster);
        let extra: Vec<_> = report.items.iter().filter(|i| i.kind == DriftKind::ExtraInConfig).collect();
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].model, "gemini-3.5-flash");
    }

    #[test]
    fn roster_drift_diff_fixture_tier_mismatch_against_real_config() {
        let entries = parse_scorecard(&load_fixture("scorecard-tier-mismatch.md")).unwrap();
        let cfg = load_conductor_config();
        let report = diff(&entries, &cfg.roster);
        let mismatches: Vec<_> = report.items.iter().filter(|i| i.kind == DriftKind::TierMismatch).collect();
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].model, "sonnet-5");
    }

    #[test]
    fn roster_drift_diff_fixture_ceiling_mismatch_against_real_config() {
        let entries = parse_scorecard(&load_fixture("scorecard-ceiling-mismatch.md")).unwrap();
        let cfg = load_conductor_config();
        let report = diff(&entries, &cfg.roster);
        let mismatches: Vec<_> = report.items.iter().filter(|i| i.kind == DriftKind::CeilingMismatch).collect();
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].model, "sonnet-5");
    }
}
