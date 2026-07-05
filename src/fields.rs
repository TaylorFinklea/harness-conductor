//! routing-field extraction: metadata first, notes-prose fallback (pure)
//!
//! Pinned rules (conductor-v1-spec, "Routing-field extraction"):
//! 1. Prefer bd metadata keys: `tier_floor` ∈ {`lead`,`senior`,`junior`},
//!    `complexity` ∈ {`S`,`M`,`L`,`XL`}, `verify_cmd` = exact shell command.
//! 2. Fallback: scan notes for `tier_floor:\s*(lead|senior|junior)` and
//!    `complexity:\s*(XL|S|M|L)(?:\s*[-–]\s*(XL|S|M|L))?` (case-insensitive).
//!    A range like `S-M` resolves to its upper bound (the max of the two
//!    values in the complexity order `S<M<L<XL`).
//!    A notes `verify_type:` line is NOT a runnable `verify_cmd`.
//! 3. Metadata present but invalid value ⇒ Untriaged (fail closed, no notes
//!    fallback for that field). Anything missing/unparseable ⇒ Untriaged.

// The public(crate) API is built ahead of its consumer in `triage.rs` (M2).
// Silence dead-code until the routing algorithm imports these types.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde_json::Value;

use crate::bd::Issue;
use crate::config::{Ceiling, Tier};

/// Routing fields extracted from a bead: `tier_floor` and `complexity` are
/// always present when `Triage::Triaged`; `verify_cmd` is optional (a missing
/// `verify_cmd` flags the item for triage downstream but does not make it
/// Untriaged — see conductor-v1-spec invariant 3). `trains_ok` is an item-level
/// opt-in that lifts the `FreeTrainsInput` repo-policy gate (bead metadata
/// `data_policy: trains-ok` — e.g. a public-dataset task on a proprietary
/// repo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutingFields {
    pub(crate) tier_floor: Tier,
    pub(crate) complexity: Ceiling,
    pub(crate) verify_cmd: Option<String>,
    pub(crate) trains_ok: bool,
}

/// Which required field was missing or unparseable. `verify_cmd` is not a
/// member — a missing `verify_cmd` is not Untriaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingField {
    TierFloor,
    Complexity,
}

/// Result of routing-field extraction. `Triaged` items have all required fields
/// and may be routed; `Untriaged` items must only be proposed (M5), never
/// dispatched as work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Triage {
    Triaged(RoutingFields),
    Untriaged { missing: Vec<MissingField> },
}

/// Extract routing fields from an `Issue`, preferring bd metadata over
/// notes-prose fallback. Pure: no IO, no allocation beyond the return value.
pub(crate) fn extract(issue: &Issue) -> Triage {
    let metadata = issue.metadata.as_ref();
    let notes = issue.notes.as_str();

    let tier_floor = extract_tier(metadata, notes);
    let complexity = extract_complexity(metadata, notes);
    let verify_cmd = extract_verify_cmd(metadata);
    let trains_ok = extract_trains_ok(metadata);

    if let (Some(tier_floor), Some(complexity)) = (tier_floor, complexity) {
        return Triage::Triaged(RoutingFields {
            tier_floor,
            complexity,
            verify_cmd,
            trains_ok,
        });
    }
    let mut missing = Vec::new();
    if tier_floor.is_none() {
        missing.push(MissingField::TierFloor);
    }
    if complexity.is_none() {
        missing.push(MissingField::Complexity);
    }
    Triage::Untriaged { missing }
}

// ---------------------------------------------------------------------------
// tier_floor
// ---------------------------------------------------------------------------

fn extract_tier(metadata: Option<&BTreeMap<String, Value>>, notes: &str) -> Option<Tier> {
    if let Some(map) = metadata {
        if let Some(value) = map.get("tier_floor") {
            // metadata present ⇒ strict parse, no notes fallback
            return parse_tier_value(value);
        }
    }
    parse_tier_in_notes(notes)
}

fn parse_tier_value(value: &Value) -> Option<Tier> {
    let s = value.as_str()?.trim();
    match s.to_ascii_lowercase().as_str() {
        "lead" => Some(Tier::Lead),
        "senior" => Some(Tier::Senior),
        "junior" => Some(Tier::Junior),
        _ => None,
    }
}

fn parse_tier_in_notes(notes: &str) -> Option<Tier> {
    let pos = find_key(notes, "tier_floor")?;
    let after_key = notes[pos + "tier_floor".len()..].trim_start();
    let after_colon = after_key.strip_prefix(':')?.trim_start();
    let (word, _rest) = read_word(after_colon);
    match word.to_ascii_lowercase().as_str() {
        "lead" => Some(Tier::Lead),
        "senior" => Some(Tier::Senior),
        "junior" => Some(Tier::Junior),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// complexity (with optional range)
// ---------------------------------------------------------------------------

fn extract_complexity(metadata: Option<&BTreeMap<String, Value>>, notes: &str) -> Option<Ceiling> {
    if let Some(map) = metadata {
        if let Some(value) = map.get("complexity") {
            return parse_complexity_value(value);
        }
    }
    parse_complexity_in_notes(notes)
}

fn parse_complexity_value(value: &Value) -> Option<Ceiling> {
    let s = value.as_str()?.trim();
    parse_complexity_token(s)
}

fn parse_complexity_in_notes(notes: &str) -> Option<Ceiling> {
    let pos = find_key(notes, "complexity")?;
    let after_key = notes[pos + "complexity".len()..].trim_start();
    let after_colon = after_key.strip_prefix(':')?.trim_start();
    let (first_word, rest) = read_word(after_colon);
    let first = parse_complexity_token(first_word)?;

    // optional range: whitespace, then '-' or '–' (en-dash), then whitespace, then second value
    let rest = rest.trim_start();
    let after_sep = if let Some(s) = rest.strip_prefix('-') {
        s
    } else if let Some(s) = rest.strip_prefix('–') {
        s
    } else {
        return Some(first);
    };
    let (second_word, _rest) = read_word(after_sep.trim_start());
    let second = parse_complexity_token(second_word)?;
    Some(max_complexity(first, second))
}

fn parse_complexity_token(s: &str) -> Option<Ceiling> {
    match s.to_ascii_uppercase().as_str() {
        "S" => Some(Ceiling::S),
        "M" => Some(Ceiling::M),
        "L" => Some(Ceiling::L),
        "XL" => Some(Ceiling::Xl),
        _ => None,
    }
}

fn max_complexity(a: Ceiling, b: Ceiling) -> Ceiling {
    let rank = |c: Ceiling| match c {
        Ceiling::S => 0,
        Ceiling::M => 1,
        Ceiling::L => 2,
        Ceiling::Xl => 3,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

// ---------------------------------------------------------------------------
// verify_cmd (metadata only — notes "verify_type" is not a runnable command)
// ---------------------------------------------------------------------------

fn extract_verify_cmd(metadata: Option<&BTreeMap<String, Value>>) -> Option<String> {
    let map = metadata?;
    let value = map.get("verify_cmd")?;
    value.as_str().map(str::to_string)
}

/// Extract the per-item `data_policy` opt-in. A bead carrying
/// `data_policy: "trains-ok"` lifts the `FreeTrainsInput` repo-policy gate
/// (lets a free-train model run on a proprietary repo for a specific item).
fn extract_trains_ok(metadata: Option<&BTreeMap<String, Value>>) -> bool {
    let Some(map) = metadata else {
        return false;
    };
    let Some(value) = map.get("data_policy") else {
        return false;
    };
    match value.as_str() {
        Some(s) => s.trim().eq_ignore_ascii_case("trains-ok"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// notes scanning helpers
// ---------------------------------------------------------------------------

/// Find the byte position of the start of `key` in `notes`, requiring that the
/// next non-whitespace character after the key is `:`. Case-insensitive.
/// Returns `None` if no qualifying occurrence is found.
fn find_key(notes: &str, key: &str) -> Option<usize> {
    let key_lower = key.to_ascii_lowercase();
    let notes_lower = notes.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = notes_lower[search_from..].find(&key_lower) {
        let abs = search_from + rel;
        let after = notes[abs + key.len()..].trim_start();
        if after.starts_with(':') {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

/// Read the longest run of ASCII alphanumeric characters from the start of
/// `s`; return the slice plus the remaining suffix.
fn read_word(s: &str) -> (&str, &str) {
    let end = s
        .as_bytes()
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .unwrap_or(s.len());
    s.split_at(end)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_issue(notes: &str, metadata: Option<BTreeMap<String, Value>>) -> Issue {
        Issue {
            id: "fixture-1".to_string(),
            title: "fixture".to_string(),
            description: String::new(),
            acceptance_criteria: String::new(),
            notes: notes.to_string(),
            status: "open".to_string(),
            priority: 1,
            issue_type: "task".to_string(),
            assignee: None,
            owner: "fixture".to_string(),
            created_at: "2026-07-01T00:00:00Z".to_string(),
            created_by: "fixture".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            started_at: None,
            labels: None,
            estimated_minutes: None,
            metadata,
            parent: None,
            dependencies: None,
            dependency_count: None,
            dependent_count: None,
            comment_count: None,
        }
    }

    fn md(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn triaged(tier: Tier, complexity: Ceiling, verify_cmd: Option<&str>) -> Triage {
        Triage::Triaged(RoutingFields {
            tier_floor: tier,
            complexity,
            verify_cmd: verify_cmd.map(str::to_string),
        })
    }

    fn untriaged(missing: &[MissingField]) -> Triage {
        Triage::Untriaged {
            missing: missing.to_vec(),
        }
    }

    // --- precedence ---

    #[test]
    fn metadata_wins_over_conflicting_notes() {
        let metadata = md(&[("tier_floor", json!("lead")), ("complexity", json!("M"))]);
        let issue = make_issue("tier_floor: junior · complexity: L", Some(metadata));
        assert_eq!(extract(&issue), triaged(Tier::Lead, Ceiling::M, None));
    }

    // --- every enum member, via metadata ---

    #[test]
    fn table_driven_all_tier_and_complexity_combinations_via_metadata() {
        let tiers = [
            (Tier::Lead, "lead"),
            (Tier::Senior, "senior"),
            (Tier::Junior, "junior"),
        ];
        let complexities = [
            (Ceiling::S, "S"),
            (Ceiling::M, "M"),
            (Ceiling::L, "L"),
            (Ceiling::Xl, "XL"),
        ];
        for (tier, tier_s) in tiers {
            for (comp, comp_s) in complexities {
                let metadata =
                    md(&[("tier_floor", json!(tier_s)), ("complexity", json!(comp_s))]);
                let issue = make_issue("", Some(metadata));
                assert_eq!(
                    extract(&issue),
                    triaged(tier, comp, None),
                    "tier_floor={tier_s} complexity={comp_s}"
                );
            }
        }
    }

    // --- every enum member, via notes ---

    #[test]
    fn table_driven_all_tier_and_complexity_combinations_via_notes() {
        let tiers = [
            (Tier::Lead, "lead"),
            (Tier::Senior, "senior"),
            (Tier::Junior, "junior"),
        ];
        let complexities = [
            (Ceiling::S, "S"),
            (Ceiling::M, "M"),
            (Ceiling::L, "L"),
            (Ceiling::Xl, "XL"),
        ];
        for (tier, tier_s) in tiers {
            for (comp, comp_s) in complexities {
                let notes = format!("tier_floor: {tier_s} · complexity: {comp_s}");
                let issue = make_issue(&notes, None);
                assert_eq!(
                    extract(&issue),
                    triaged(tier, comp, None),
                    "tier_floor={tier_s} complexity={comp_s}"
                );
            }
        }
    }

    // --- range separators: hyphen (-, U+002D) and en-dash (–, U+2013) ---

    #[test]
    fn notes_range_hyphen_resolves_to_upper_bound() {
        let cases = [
            ("S-M", Ceiling::M),
            ("M-L", Ceiling::L),
            ("L-XL", Ceiling::Xl),
            ("S-XL", Ceiling::Xl),
        ];
        for (range, expected) in cases {
            let notes = format!("tier_floor: senior · complexity: {range}");
            let issue = make_issue(&notes, None);
            assert_eq!(
                extract(&issue),
                triaged(Tier::Senior, expected, None),
                "range={range}"
            );
        }
    }

    #[test]
    fn notes_range_endash_resolves_to_upper_bound() {
        let cases = [
            ("S–M", Ceiling::M),
            ("M–L", Ceiling::L),
            ("L–XL", Ceiling::Xl),
            ("S–XL", Ceiling::Xl),
        ];
        for (range, expected) in cases {
            let notes = format!("tier_floor: senior · complexity: {range}");
            let issue = make_issue(&notes, None);
            assert_eq!(
                extract(&issue),
                triaged(Tier::Senior, expected, None),
                "range={range}"
            );
        }
    }

    #[test]
    fn notes_range_upper_bound_is_max_regardless_of_order() {
        // "XL-S" is a malformed range; spec says "upper bound" => the larger value (XL).
        let issue = make_issue("tier_floor: senior · complexity: XL-S", None);
        assert_eq!(
            extract(&issue),
            triaged(Tier::Senior, Ceiling::Xl, None)
        );
    }

    #[test]
    fn notes_complexity_with_extra_whitespace_around_separator() {
        let issue = make_issue("tier_floor: senior · complexity: S - M", None);
        assert_eq!(extract(&issue), triaged(Tier::Senior, Ceiling::M, None));
        let issue = make_issue("tier_floor: senior · complexity: S  –  M", None);
        assert_eq!(extract(&issue), triaged(Tier::Senior, Ceiling::M, None));
    }

    // --- fail-closed: metadata present but invalid -> Untriaged ---

    #[test]
    fn invalid_tier_in_metadata_means_untriaged_no_notes_fallback() {
        // complexity supplied valid in metadata so the test isolates tier_floor's behavior.
        let metadata = md(&[("tier_floor", json!("boss")), ("complexity", json!("M"))]);
        let issue = make_issue("tier_floor: junior", Some(metadata));
        assert_eq!(extract(&issue), untriaged(&[MissingField::TierFloor]));
    }

    #[test]
    fn invalid_complexity_in_metadata_means_untriaged_no_notes_fallback() {
        let metadata = md(&[("tier_floor", json!("senior")), ("complexity", json!("XX"))]);
        let issue = make_issue("complexity: S", Some(metadata));
        assert_eq!(extract(&issue), untriaged(&[MissingField::Complexity]));
    }

    #[test]
    fn non_string_tier_in_metadata_means_untriaged() {
        // complexity supplied valid in metadata so the test isolates tier_floor's behavior.
        let metadata = md(&[("tier_floor", json!(5)), ("complexity", json!("M"))]);
        let issue = make_issue("tier_floor: senior", Some(metadata));
        assert_eq!(extract(&issue), untriaged(&[MissingField::TierFloor]));
    }

    // --- garbage/empty notes => Untriaged ---

    #[test]
    fn garbage_or_empty_notes_means_untriaged() {
        // No tier_floor, no complexity, and no rescue via partial matches.
        let cases = [
            "",
            "asdfasdf",
            "tier_floor: boss",
            "complexity: XX",
            "verify_type: cargo test",
        ];
        for notes in cases {
            let issue = make_issue(notes, None);
            assert_eq!(
                extract(&issue),
                untriaged(&[MissingField::TierFloor, MissingField::Complexity]),
                "notes={notes:?}"
            );
        }
    }

    #[test]
    fn partial_notes_only_tier_floor_means_untriaged_complexity_missing() {
        let issue = make_issue("tier_floor: senior", None);
        assert_eq!(extract(&issue), untriaged(&[MissingField::Complexity]));
    }

    #[test]
    fn partial_notes_only_complexity_means_untriaged_tier_floor_missing() {
        let issue = make_issue("complexity: M", None);
        assert_eq!(extract(&issue), untriaged(&[MissingField::TierFloor]));
    }

    // --- the spec's headline fixture ---

    #[test]
    fn real_tesela_notes_format_yields_senior_m_no_verify_cmd() {
        // Exact format from the task data and the live tesela notes.
        let notes =
            "tier_floor: senior · complexity: S-M · verify_type: wrangler dev + cargo test";
        let issue = make_issue(notes, None);
        assert_eq!(extract(&issue), triaged(Tier::Senior, Ceiling::M, None));
    }

    // --- verify_cmd provenance: metadata only ---

    #[test]
    fn verify_cmd_comes_only_from_metadata_not_from_notes() {
        let metadata = md(&[("tier_floor", json!("senior")), ("complexity", json!("M"))]);
        let issue = make_issue(
            "verify_type: cargo test · verify_cmd: bogus shell injection",
            Some(metadata),
        );
        assert_eq!(extract(&issue), triaged(Tier::Senior, Ceiling::M, None));
    }

    #[test]
    fn verify_cmd_from_metadata_round_trips() {
        let metadata = md(&[
            ("tier_floor", json!("senior")),
            ("complexity", json!("M")),
            ("verify_cmd", json!("cargo test fields")),
        ]);
        let issue = make_issue("", Some(metadata));
        assert_eq!(
            extract(&issue),
            triaged(Tier::Senior, Ceiling::M, Some("cargo test fields"))
        );
    }

    // --- mixed: each field independently chooses metadata or notes ---

    #[test]
    fn tier_floor_from_notes_complexity_from_metadata() {
        let metadata = md(&[("complexity", json!("L"))]);
        let issue = make_issue("tier_floor: lead", Some(metadata));
        assert_eq!(extract(&issue), triaged(Tier::Lead, Ceiling::L, None));
    }

    #[test]
    fn complexity_from_notes_tier_floor_from_metadata() {
        let metadata = md(&[("tier_floor", json!("junior"))]);
        let issue = make_issue("complexity: S-XL", Some(metadata));
        assert_eq!(
            extract(&issue),
            triaged(Tier::Junior, Ceiling::Xl, None)
        );
    }

    // --- case insensitivity ---

    #[test]
    fn notes_parsing_is_case_insensitive() {
        let cases = [
            ("TIER_FLOOR: LEAD · COMPLEXITY: XL", Tier::Lead, Ceiling::Xl),
            (
                "Tier_Floor: Senior · Complexity: S",
                Tier::Senior,
                Ceiling::S,
            ),
            (
                "tier_floor: junior · complexity: m-l",
                Tier::Junior,
                Ceiling::L,
            ),
            (
                "tier_floor: SeNiOr · complexity: S–M",
                Tier::Senior,
                Ceiling::M,
            ),
        ];
        for (notes, tier, comp) in cases {
            let issue = make_issue(notes, None);
            assert_eq!(
                extract(&issue),
                triaged(tier, comp, None),
                "notes={notes:?}"
            );
        }
    }

    // --- empty metadata => pure notes path ---

    #[test]
    fn empty_metadata_map_falls_back_to_notes() {
        let metadata: BTreeMap<String, Value> = BTreeMap::new();
        let issue = make_issue("tier_floor: lead · complexity: XL", Some(metadata));
        assert_eq!(extract(&issue), triaged(Tier::Lead, Ceiling::Xl, None));
    }

    // --- verify_cmd is optional, not Untriaged-causing ---

    #[test]
    fn triaged_without_verify_cmd_is_still_triaged() {
        let metadata = md(&[("tier_floor", json!("senior")), ("complexity", json!("M"))]);
        let issue = make_issue("", Some(metadata));
        assert_eq!(extract(&issue), triaged(Tier::Senior, Ceiling::M, None));
    }
}
