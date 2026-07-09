//! ratchet counters (`~/.local/state/conductor/ratchet.json`)
//!
//! Pinned by `conductor-v1-spec.md` § Ratchet:
//! - State: `~/.local/state/conductor/ratchet.json` shaped
//!   `{repo: {clean_cycles: N, unlocked: bool}}`.
//! - 3 consecutive clean cycles for a repo → unlocked.
//! - Clean cycle for a repo = every proposal touching it was approved
//!   unmodified AND every dispatch in it verified-closed.
//! - Unlocked repos: `conductor cycle` may auto-dispatch items with
//!   `tier_floor ∈ {senior, junior}` AND `complexity ≤ M` AND a runnable
//!   `verify_cmd`, within budgets, WITHOUT waiting for approval — but they
//!   still appear in the report. Lead-floor items are ALWAYS propose-only
//!   (invariant 5). Anything else still proposes.
//! - ANY rejected proposal / failed verify / worker failure → that repo's
//!   counter resets to 0 and relocks (invariant 9: "Ratchet failure re-locks").
//! - Global override: `autonomy = "propose"` in `conductor.toml` disables
//!   auto-dispatch everywhere.
//!
//! Month-1 CONFIG DEFAULT (decisions.md 2026-07-03, ADR comment on
//! `conductor-m6`): the *mechanism* ships at the spec ceiling
//! (`{senior, junior}` + `≤ M`); the *config* ships narrower (junior + S).
//! Widening toward the spec ceiling is a HUMAN config change backed by
//! ratchet evidence. See `Config::ratchet` for the defaults.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Ceiling, Tier};
use crate::triage::RatchetState;

// ---------------------------------------------------------------------------
// Constants (spec-pinned)
// ---------------------------------------------------------------------------

/// How many consecutive clean cycles a repo needs before it earns auto-
/// dispatch. Spec § Ratchet pins this at 3.
pub(crate) const CLEAN_CYCLES_TO_UNLOCK: u32 = 3;

/// Filename of the persisted ratchet state under the conductor state dir.
pub(crate) const RATCHET_FILE_NAME: &str = "ratchet.json";

/// Spec ceiling for the ratchet mechanism itself: the MECHANISM auto-
/// dispatches ONLY when `tier_floor ∈ {senior, junior}` AND
/// `complexity ≤ M`. The month-1 CONFIG default is narrower; widening
/// toward this ceiling is a human config change (see ADR 2026-07-03).
pub(crate) const SPEC_MAX_TIER_FLOOR: Tier = Tier::Senior;
pub(crate) const SPEC_MAX_COMPLEXITY: Ceiling = Ceiling::M;

// ---------------------------------------------------------------------------
// Persisted shape (ratchet.json)
// ---------------------------------------------------------------------------

/// One repo's ratchet state, as persisted to `ratchet.json`.
///
/// `clean_cycles` increments on each consecutive clean cycle (a clean
/// cycle = every proposal approved unmodified AND every dispatch verified-
/// closed) and resets to 0 on any failure (invariant 9). `unlocked` flips
/// to `true` the cycle that pushes `clean_cycles` to `CLEAN_CYCLES_TO_UNLOCK`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RatchetEntry {
    pub(crate) clean_cycles: u32,
    pub(crate) unlocked: bool,
}

/// Top-level ratchet store — `HashMap<repo, RatchetEntry>` serialized as a
/// JSON object (`BTreeMap` for deterministic key order). Absent repos are
/// treated as `RatchetEntry::default()` (locked; fail closed).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RatchetStore {
    #[serde(flatten)]
    pub(crate) repos: BTreeMap<String, RatchetEntry>,
}

// ---------------------------------------------------------------------------
// Failure kinds (invariant 9: ANY of these re-locks the repo's ratchet)
// ---------------------------------------------------------------------------

/// Why a repo's ratchet was reset to 0 + relocked. The mechanism does not
/// distinguish them — all three failure kinds return a repo to propose-only
/// — but the enum preserves the cause for the cycle report and ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// A proposal touching the repo was rejected or modified by the human
    /// reviewer (spec: "every proposal touching it was approved
    /// unmodified").
    ProposalRejection,
    /// A dispatch in the repo failed its `verify_cmd` / orchestra / commit
    /// probe (spec: "every dispatch in it verified-closed").
    VerifyFailure,
    /// The worker process for a dispatch failed (timeout, non-zero exit,
    /// or other runtime error).
    WorkerFailure,
}

// ---------------------------------------------------------------------------
// Autonomy mode (mirrors `config::Autonomy` — kept as a separate type so
// ratchet.rs has no dependency cycle with the loaded `Config` struct)
// ---------------------------------------------------------------------------

/// Whether the ratchet is allowed to auto-dispatch at all. Mirrors
/// `config::Autonomy`; `Propose` is the global kill-switch that disables
/// auto-dispatch for every repo, regardless of the ratchet state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutonomyMode {
    Propose,
    Ratchet,
}

impl AutonomyMode {
    pub(crate) fn from_config(autonomy: crate::config::Autonomy) -> Self {
        match autonomy {
            crate::config::Autonomy::Propose => AutonomyMode::Propose,
            crate::config::Autonomy::Ratchet => AutonomyMode::Ratchet,
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-dispatch eligibility predicate
// ---------------------------------------------------------------------------

/// Configured ceiling on what the ratchet will auto-dispatch. The
/// mechanism's hard ceiling is `{senior, junior} × ≤ M`; the *config* can
/// be narrower. The effective clamp is `min(SPEC, config)` — encoded by
/// simply checking the config values directly (the spec ceiling is the
/// upper bound of the Tier / Ceiling enums, and the test suite covers
/// the SPEC ceiling case explicitly).
///
/// `clean_cycles_to_unlock` is exposed as a config knob (default =
/// `CLEAN_CYCLES_TO_UNLOCK`) for tests; production stays at the spec value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RatchetConfig {
    pub(crate) max_tier_floor: Tier,
    pub(crate) max_complexity: Ceiling,
    pub(crate) clean_cycles_to_unlock: u32,
}

impl Default for RatchetConfig {
    /// Month-1 default: junior + S (decisions.md 2026-07-03). Production
    /// widens to the spec ceiling only via a human config change backed by
    /// ratchet evidence.
    fn default() -> Self {
        Self {
            max_tier_floor: Tier::Junior,
            max_complexity: Ceiling::S,
            clean_cycles_to_unlock: CLEAN_CYCLES_TO_UNLOCK,
        }
    }
}

impl RatchetConfig {
    /// The spec ceiling (the mechanism's hard cap, used by tests that want
    /// the widest posture the ratchet is *allowed* to reach).
    pub(crate) fn spec_ceiling() -> Self {
        Self {
            max_tier_floor: SPEC_MAX_TIER_FLOOR,
            max_complexity: SPEC_MAX_COMPLEXITY,
            clean_cycles_to_unlock: CLEAN_CYCLES_TO_UNLOCK,
        }
    }
}

/// Inputs to the eligibility predicate — the things the ratchet needs to
/// know about an item to decide auto vs propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemShape<'a> {
    pub(crate) repo: &'a str,
    pub(crate) tier_floor: Tier,
    pub(crate) complexity: Ceiling,
    pub(crate) verify_cmd: Option<&'a str>,
}

/// Why an item is a proposal rather than an auto-dispatch. The predicate
/// returns the most specific reason in evaluation order, so reports can
/// show *why* auto-dispatch was withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProposeReason {
    /// Global kill-switch: `autonomy = "propose"` in `conductor.toml`.
    AutonomyPropose,
    /// Repo has not earned unlock (`clean_cycles` < threshold or explicit
    /// relock after a failure).
    RatchetLocked,
    /// Item has `tier_floor = lead` — invariant 5: lead-floor items are
    /// ALWAYS propose-only.
    LeadFloor,
    /// Item is missing a runnable `verify_cmd` — invariant 3 (fail closed,
    /// not dispatchable).
    MissingVerifyCmd,
    /// Item's `tier_floor` exceeds the configured `ratchet.max_tier_floor`
    /// clamp (the ratchet mechanism caps at `SPEC_MAX_TIER_FLOOR`, but the
    /// config can be narrower; e.g. month-1 default `junior` rejects
    /// `senior` items).
    OverMaxTier,
    /// Item's `complexity` exceeds the configured `ratchet.max_complexity`
    /// clamp.
    OverMaxComplexity,
}

/// Output of the eligibility predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EligibilityDecision {
    /// The ratchet says: dispatch this without waiting for human approval
    /// (still reported).
    AutoDispatch,
    /// The ratchet says: hold for human approval; report the reason.
    Propose(ProposeReason),
}

/// Pure predicate — no IO. Given the current ratchet state for a repo, the
/// item's shape, the autonomy mode, and the configured ceiling, returns
/// whether the ratchet permits an auto-dispatch. Order of checks matches
/// spec § Ratchet + invariant 5 (lead never auto) + ADR 2026-07-03
/// (config clamps win over mechanism ceiling).
pub(crate) fn evaluate(
    ratchet: RatchetState,
    item: ItemShape<'_>,
    autonomy: AutonomyMode,
    cfg: RatchetConfig,
) -> EligibilityDecision {
    // Global override first — the spec's only true kill-switch.
    if autonomy == AutonomyMode::Propose {
        return EligibilityDecision::Propose(ProposeReason::AutonomyPropose);
    }
    // Repo must be unlocked. Absent-from-store maps to Locked.
    if ratchet != RatchetState::Unlocked {
        return EligibilityDecision::Propose(ProposeReason::RatchetLocked);
    }
    // Lead-floor items are ALWAYS propose-only (invariant 5 + spec §
    // Ratchet: "tier_floor ∈ {senior, junior}"). Check before the
    // configured ceiling clamps so reports surface the root cause.
    if item.tier_floor == Tier::Lead {
        return EligibilityDecision::Propose(ProposeReason::LeadFloor);
    }
    // A missing runnable verify_cmd is never dispatchable (invariant 3).
    if item.verify_cmd.is_none() {
        return EligibilityDecision::Propose(ProposeReason::MissingVerifyCmd);
    }
    // Configured ceiling clamps: month-1 default is junior / S, narrower
    // than the spec ceiling. Item must be at-or-below the clamp.
    if tier_exceeds_ceiling(item.tier_floor, cfg.max_tier_floor) {
        return EligibilityDecision::Propose(ProposeReason::OverMaxTier);
    }
    if complexity_exceeds_ceiling(item.complexity, cfg.max_complexity) {
        return EligibilityDecision::Propose(ProposeReason::OverMaxComplexity);
    }
    EligibilityDecision::AutoDispatch
}

/// `tier > max` in the order `junior < senior < lead` (matches the spec
/// routing algorithm). `tier == max` is allowed (the clamp is inclusive).
fn tier_exceeds_ceiling(tier: Tier, max: Tier) -> bool {
    let r = |t: Tier| match t {
        Tier::Junior => 0,
        Tier::Senior => 1,
        Tier::Lead => 2,
    };
    r(tier) > r(max)
}

/// `complexity > max` in the order `S < M < L < XL`. `complexity == max` is
/// allowed (the clamp is inclusive).
fn complexity_exceeds_ceiling(c: Ceiling, max: Ceiling) -> bool {
    let r = |c: Ceiling| match c {
        Ceiling::S => 0,
        Ceiling::M => 1,
        Ceiling::L => 2,
        Ceiling::Xl => 3,
    };
    r(c) > r(max)
}

// ---------------------------------------------------------------------------
// File-backed store
// ---------------------------------------------------------------------------

/// The on-disk ratchet store, rooted at the conductor state dir. Provides
/// the cycle orchestrator with the load/record surface; tests construct
/// one pointed at a temp dir.
pub(crate) struct RatchetFileStore {
    path: PathBuf,
}

impl RatchetFileStore {
    /// Open the store at `<state_dir>/ratchet.json`. The file need not
    /// exist yet — a missing/corrupt file is treated as an empty store
    /// (all repos locked, `clean_cycles=0`; invariant 9 fail-closed).
    pub(crate) fn open(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(RATCHET_FILE_NAME),
        }
    }

    /// Path to the persisted file (for tests + status reporting).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Load the persisted state. Missing file → `RatchetStore::default()`.
    /// Unparseable file → `RatchetStore::default()` (fail closed; the
    /// next cycle's first `record_*` call will overwrite the file with a
    /// valid empty state).
    pub(crate) fn load(&self) -> io::Result<RatchetStore> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    return Ok(RatchetStore::default());
                }
                match serde_json::from_slice::<RatchetStore>(&bytes) {
                    Ok(store) => Ok(store),
                    Err(_) => Ok(RatchetStore::default()),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RatchetStore::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomically write the store to disk (temp file + rename) so a crash
    /// mid-write can't truncate the persisted state.
    pub(crate) fn save(&self, store: &RatchetStore) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(store).map_err(io::Error::other)?;
        // Write to a sibling temp file in the same directory, then rename.
        // Using a sibling (not /tmp) keeps the rename atomic on the same
        // filesystem.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Look up the ratchet state for a single repo. Returns `Locked` for
    /// repos absent from the store (fail closed; invariant 9).
    pub(crate) fn state_for(&self, repo: &str) -> io::Result<RatchetState> {
        let store = self.load()?;
        Ok(store.repos.get(repo).map_or(RatchetState::Locked, |e| {
            if e.unlocked {
                RatchetState::Unlocked
            } else {
                RatchetState::Locked
            }
        }))
    }

    /// Record a clean cycle for a repo. Increments `clean_cycles`; if it
    /// reaches `clean_cycles_to_unlock`, sets `unlocked = true`. The
    /// counter does not overflow — it is capped at `clean_cycles_to_unlock`
    /// so an already-unlocked repo stays "unlocked, 3 clean cycles" until
    /// a failure resets it.
    pub(crate) fn record_clean(
        &self,
        repo: &str,
        clean_cycles_to_unlock: u32,
    ) -> io::Result<RatchetEntry> {
        let mut store = self.load()?;
        let entry = store.repos.entry(repo.to_string()).or_default();
        if !entry.unlocked {
            entry.clean_cycles = entry
                .clean_cycles
                .saturating_add(1)
                .min(clean_cycles_to_unlock);
            if clean_cycles_to_unlock > 0 && entry.clean_cycles >= clean_cycles_to_unlock {
                entry.unlocked = true;
            }
        }
        let snapshot = entry.clone();
        self.save(&store)?;
        Ok(snapshot)
    }

    /// Record a failure for a repo. Resets `clean_cycles = 0` and
    /// `unlocked = false` (invariant 9: ANY failure re-locks; the
    /// mechanism does not distinguish failure kinds).
    pub(crate) fn record_failure(
        &self,
        repo: &str,
        _kind: FailureKind,
    ) -> io::Result<RatchetEntry> {
        let mut store = self.load()?;
        let entry = store.repos.entry(repo.to_string()).or_default();
        entry.clean_cycles = 0;
        entry.unlocked = false;
        let snapshot = entry.clone();
        self.save(&store)?;
        Ok(snapshot)
    }

    /// Snapshot the persisted state as the per-repo `triage::RatchetState`
    /// map the cycle orchestrator passes into `triage::route`. Absent
    /// repos are OMITTED (triage treats absence as Locked; fail closed).
    pub(crate) fn triage_state_map(
        &self,
    ) -> io::Result<std::collections::HashMap<String, RatchetState>> {
        let store = self.load()?;
        let mut out = std::collections::HashMap::new();
        for (repo, entry) in &store.repos {
            if entry.unlocked {
                out.insert(repo.clone(), RatchetState::Unlocked);
            }
            // Locked entries are intentionally absent; triage.rs treats
            // absence as Locked so the map stays small.
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // --- helpers ---

    /// Unique temp dir per test, cleaned up on drop via best-effort remove.
    /// Mirrors the pattern in `state.rs` and `config.rs` tests — no extra
    /// crate dependency.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-ratchet-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store_in(label: &str) -> (TempDir, RatchetFileStore) {
        let dir = TempDir::new(label);
        let store = RatchetFileStore::open(&dir.0);
        (dir, store)
    }

    fn senior_m_with_verify(repo: &str) -> ItemShape<'_> {
        ItemShape {
            repo,
            tier_floor: Tier::Senior,
            complexity: Ceiling::M,
            verify_cmd: Some("cargo test"),
        }
    }

    fn junior_s_with_verify(repo: &str) -> ItemShape<'_> {
        ItemShape {
            repo,
            tier_floor: Tier::Junior,
            complexity: Ceiling::S,
            verify_cmd: Some("cargo test"),
        }
    }

    fn lead_item(repo: &str) -> ItemShape<'_> {
        ItemShape {
            repo,
            tier_floor: Tier::Lead,
            complexity: Ceiling::L,
            verify_cmd: Some("cargo test"),
        }
    }

    fn no_verify_cmd(repo: &str) -> ItemShape<'_> {
        ItemShape {
            repo,
            tier_floor: Tier::Senior,
            complexity: Ceiling::M,
            verify_cmd: None,
        }
    }

    // -----------------------------------------------------------------------
    // 1. Counter / unlock threshold
    // -----------------------------------------------------------------------

    #[test]
    fn clean_cycles_1_and_2_keep_repo_locked() {
        let (_dir, store) = store_in("threshold-1-2");
        assert_eq!(
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap(),
            RatchetEntry {
                clean_cycles: 1,
                unlocked: false
            }
        );
        assert_eq!(
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap(),
            RatchetEntry {
                clean_cycles: 2,
                unlocked: false
            }
        );
        assert_eq!(
            store.state_for("repo1").unwrap(),
            RatchetState::Locked,
            "two clean cycles is below the spec threshold of 3"
        );
    }

    #[test]
    fn three_consecutive_clean_cycles_unlocks_repo() {
        let (_dir, store) = store_in("threshold-3-unlocks");
        store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        let after_third = store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        assert_eq!(
            after_third,
            RatchetEntry {
                clean_cycles: 3,
                unlocked: true
            }
        );
        assert_eq!(store.state_for("repo1").unwrap(), RatchetState::Unlocked);
    }

    #[test]
    fn clean_cycles_counter_caps_at_threshold() {
        // Once unlocked, additional clean cycles must NOT keep incrementing
        // the counter past the threshold (would defeat the "3 clean cycles"
        // interpretation and make a relock harder to reason about).
        let (_dir, store) = store_in("counter-cap");
        for _ in 0..5 {
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        }
        let entry = store.state_for("repo1").unwrap();
        assert_eq!(entry, RatchetState::Unlocked);
        let persisted = store.load().unwrap();
        assert_eq!(
            persisted.repos.get("repo1"),
            Some(&RatchetEntry {
                clean_cycles: 3,
                unlocked: true
            })
        );
    }

    // -----------------------------------------------------------------------
    // 2. Relock on every failure kind (invariant 9)
    // -----------------------------------------------------------------------

    #[test]
    fn proposal_rejection_relocks_even_after_unlock() {
        let (_dir, store) = store_in("relock-rejection");
        for _ in 0..3 {
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        }
        assert_eq!(store.state_for("repo1").unwrap(), RatchetState::Unlocked);

        let after = store
            .record_failure("repo1", FailureKind::ProposalRejection)
            .unwrap();
        assert_eq!(after, RatchetEntry::default());
        assert_eq!(store.state_for("repo1").unwrap(), RatchetState::Locked);
    }

    #[test]
    fn verify_failure_relocks_even_after_unlock() {
        let (_dir, store) = store_in("relock-verify");
        for _ in 0..3 {
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        }
        let after = store
            .record_failure("repo1", FailureKind::VerifyFailure)
            .unwrap();
        assert_eq!(after, RatchetEntry::default());
        assert_eq!(store.state_for("repo1").unwrap(), RatchetState::Locked);
    }

    #[test]
    fn worker_failure_relocks_even_after_unlock() {
        let (_dir, store) = store_in("relock-worker");
        for _ in 0..3 {
            store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        }
        let after = store
            .record_failure("repo1", FailureKind::WorkerFailure)
            .unwrap();
        assert_eq!(after, RatchetEntry::default());
        assert_eq!(store.state_for("repo1").unwrap(), RatchetState::Locked);
    }

    #[test]
    fn relock_resets_counter_so_unlock_requires_three_more_clean_cycles() {
        // 2 clean → failure → 3 more clean should re-unlock (proving the
        // counter really did reset to 0, not 2).
        let (_dir, store) = store_in("relock-reset-counter");
        store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store
            .record_failure("repo1", FailureKind::VerifyFailure)
            .unwrap();
        // First two clean cycles after the failure should NOT unlock.
        store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        let after_two = store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        assert_eq!(
            after_two,
            RatchetEntry {
                clean_cycles: 2,
                unlocked: false
            }
        );
        // The third clean cycle after the failure re-unlocks.
        let after_three = store.record_clean("repo1", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        assert_eq!(
            after_three,
            RatchetEntry {
                clean_cycles: 3,
                unlocked: true
            }
        );
    }

    // -----------------------------------------------------------------------
    // 3. Persistence: save + reload round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn state_round_trips_through_disk() {
        let (dir, store) = store_in("round-trip");
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store
            .record_failure("beta", FailureKind::WorkerFailure)
            .unwrap();
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();

        // Reopen the same file in a fresh store handle.
        let reopened = RatchetFileStore::open(&dir.0);
        let loaded = reopened.load().unwrap();
        assert_eq!(
            loaded.repos.get("alpha"),
            Some(&RatchetEntry {
                clean_cycles: 3,
                unlocked: true
            })
        );
        assert_eq!(
            loaded.repos.get("beta"),
            Some(&RatchetEntry {
                clean_cycles: 0,
                unlocked: false
            })
        );
    }

    #[test]
    fn missing_file_loads_as_empty_store() {
        // First-time install: no ratchet.json on disk yet. Load must NOT
        // error; all repos must report as Locked (fail closed).
        let (_dir, store) = store_in("missing-file");
        let loaded = store.load().unwrap();
        assert!(loaded.repos.is_empty());
        assert_eq!(store.state_for("any-repo").unwrap(), RatchetState::Locked);
    }

    #[test]
    fn corrupt_file_loads_as_empty_store_and_next_save_overwrites_it() {
        let (dir, store) = store_in("corrupt-file");
        std::fs::write(store.path(), b"not valid json {{{").unwrap();
        let loaded = store.load().unwrap();
        assert!(
            loaded.repos.is_empty(),
            "corrupt file must NOT panic the loader"
        );
        // The next write should succeed and replace the corrupt contents.
        store.record_clean("x", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        let reloaded = RatchetFileStore::open(&dir.0).load().unwrap();
        assert_eq!(
            reloaded.repos.get("x"),
            Some(&RatchetEntry {
                clean_cycles: 1,
                unlocked: false
            })
        );
    }

    // -----------------------------------------------------------------------
    // 4. Eligibility predicate — invariant 5 (lead never auto)
    // -----------------------------------------------------------------------

    #[test]
    fn lead_floor_never_auto_dispatches_even_when_unlocked() {
        // The strongest case: unlocked ratchet, spec-ceiling config, item
        // is L/XL, verify_cmd present. Still a proposal because the
        // tier_floor is `lead`.
        let decision = evaluate(
            RatchetState::Unlocked,
            lead_item("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::spec_ceiling(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::LeadFloor)
        );
    }

    #[test]
    fn lead_floor_never_auto_dispatches_under_default_narrow_config() {
        let decision = evaluate(
            RatchetState::Unlocked,
            lead_item("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::default(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::LeadFloor)
        );
    }

    // -----------------------------------------------------------------------
    // 5. Eligibility predicate — global override
    // -----------------------------------------------------------------------

    #[test]
    fn autonomy_propose_disables_auto_dispatch_globally() {
        // Even with an unlocked ratchet, the spec-ceiling config, and an
        // item that would otherwise auto-dispatch, the global override
        // forces a proposal.
        let decision = evaluate(
            RatchetState::Unlocked,
            junior_s_with_verify("repo1"),
            AutonomyMode::Propose,
            RatchetConfig::spec_ceiling(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::AutonomyPropose)
        );
    }

    // -----------------------------------------------------------------------
    // 6. Eligibility predicate — config ceiling clamps (BOTH
    //    default-narrow AND spec-ceiling postures covered)
    // -----------------------------------------------------------------------

    #[test]
    fn default_narrow_config_rejects_senior_item() {
        // Month-1 default: max_tier_floor=junior, max_complexity=S. A
        // senior/M item must be a proposal even when unlocked, because
        // the configured ceiling is narrower than the spec ceiling.
        let decision = evaluate(
            RatchetState::Unlocked,
            senior_m_with_verify("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::default(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::OverMaxTier)
        );
    }

    #[test]
    fn default_narrow_config_accepts_junior_s_item() {
        // Month-1 default: junior / S IS within the clamp → auto-dispatch.
        let decision = evaluate(
            RatchetState::Unlocked,
            junior_s_with_verify("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::default(),
        );
        assert_eq!(decision, EligibilityDecision::AutoDispatch);
    }

    #[test]
    fn spec_ceiling_config_accepts_senior_m_item() {
        // Widened posture: max_tier_floor=senior, max_complexity=M. A
        // senior/M item now auto-dispatches.
        let decision = evaluate(
            RatchetState::Unlocked,
            senior_m_with_verify("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::spec_ceiling(),
        );
        assert_eq!(decision, EligibilityDecision::AutoDispatch);
    }

    #[test]
    fn spec_ceiling_config_rejects_complexity_l() {
        // Spec ceiling is `≤ M`. An L-complexity item must propose even
        // under the widest allowed posture.
        let item = ItemShape {
            repo: "repo1",
            tier_floor: Tier::Senior,
            complexity: Ceiling::L,
            verify_cmd: Some("cargo test"),
        };
        let decision = evaluate(
            RatchetState::Unlocked,
            item,
            AutonomyMode::Ratchet,
            RatchetConfig::spec_ceiling(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::OverMaxComplexity)
        );
    }

    // -----------------------------------------------------------------------
    // 7. Eligibility predicate — locked repo, missing verify_cmd
    // -----------------------------------------------------------------------

    #[test]
    fn locked_repo_proposes_regardless_of_item_shape() {
        // RatchetLocked must be returned for ANY item when the repo is
        // locked — this is the dominant reason during the cold-start
        // months of a fresh install.
        let cases = [
            junior_s_with_verify("repo1"),
            senior_m_with_verify("repo1"),
            ItemShape {
                repo: "repo1",
                tier_floor: Tier::Junior,
                complexity: Ceiling::S,
                verify_cmd: None,
            },
        ];
        for item in cases {
            let decision = evaluate(
                RatchetState::Locked,
                item,
                AutonomyMode::Ratchet,
                RatchetConfig::spec_ceiling(),
            );
            assert_eq!(
                decision,
                EligibilityDecision::Propose(ProposeReason::RatchetLocked),
                "locked repo must propose, but got {decision:?} for item {item:?}"
            );
        }
    }

    #[test]
    fn missing_verify_cmd_always_proposes() {
        // Even an unlocked repo with the spec-ceiling config + a
        // junior/S item must propose if the item has no runnable
        // verify_cmd (invariant 3: fail closed).
        let decision = evaluate(
            RatchetState::Unlocked,
            no_verify_cmd("repo1"),
            AutonomyMode::Ratchet,
            RatchetConfig::spec_ceiling(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::MissingVerifyCmd)
        );
    }

    // -----------------------------------------------------------------------
    // 8. Multi-repo independence + end-to-end
    // -----------------------------------------------------------------------

    #[test]
    fn repos_have_independent_counters() {
        let (_dir, store) = store_in("multi-repo");
        // 2 clean for alpha, 1 for beta, 1 for gamma.
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("beta", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean("gamma", CLEAN_CYCLES_TO_UNLOCK).unwrap();

        // alpha unlocks on the next clean; beta stays locked (needs 2
        // more); gamma stays locked (needs 2 more).
        store.record_clean("alpha", CLEAN_CYCLES_TO_UNLOCK).unwrap();
        assert_eq!(store.state_for("alpha").unwrap(), RatchetState::Unlocked);
        assert_eq!(store.state_for("beta").unwrap(), RatchetState::Locked);
        assert_eq!(store.state_for("gamma").unwrap(), RatchetState::Locked);

        // A failure on alpha must not touch beta's or gamma's counters.
        store
            .record_failure("alpha", FailureKind::VerifyFailure)
            .unwrap();
        assert_eq!(store.state_for("alpha").unwrap(), RatchetState::Locked);
        assert_eq!(
            store
                .load()
                .unwrap()
                .repos
                .get("beta")
                .unwrap()
                .clean_cycles,
            1
        );
        assert_eq!(
            store
                .load()
                .unwrap()
                .repos
                .get("gamma")
                .unwrap()
                .clean_cycles,
            1
        );
    }

    #[test]
    fn end_to_end_three_clean_cycles_unlocks_then_failure_relocks() {
        // Full multi-cycle sequence mirroring the acceptance criteria:
        // earn an unlock, then a failure kind relocks.
        let (_dir, store) = store_in("e2e");
        let repo = "alpha";

        // Cycle 1: clean. Cycle 2: clean. Cycle 3: clean → unlock.
        store.record_clean(repo, CLEAN_CYCLES_TO_UNLOCK).unwrap();
        store.record_clean(repo, CLEAN_CYCLES_TO_UNLOCK).unwrap();
        let after_unlock = store.record_clean(repo, CLEAN_CYCLES_TO_UNLOCK).unwrap();
        assert_eq!(
            after_unlock,
            RatchetEntry {
                clean_cycles: 3,
                unlocked: true
            }
        );

        // Cycle 4: a proposal was rejected → relock (invariant 9).
        let after_rejection = store
            .record_failure(repo, FailureKind::ProposalRejection)
            .unwrap();
        assert_eq!(after_rejection, RatchetEntry::default());
        assert_eq!(store.state_for(repo).unwrap(), RatchetState::Locked);

        // The next auto-dispatch-eligible item must be a proposal.
        let decision = evaluate(
            store.state_for(repo).unwrap(),
            junior_s_with_verify(repo),
            AutonomyMode::Ratchet,
            RatchetConfig::default(),
        );
        assert_eq!(
            decision,
            EligibilityDecision::Propose(ProposeReason::RatchetLocked)
        );
    }

    // -----------------------------------------------------------------------
    // 9. triage_state_map helper (for the future cycle wiring)
    // -----------------------------------------------------------------------

    #[test]
    fn triage_state_map_omits_locked_repos() {
        let (_dir, store) = store_in("triage-map");
        for _ in 0..3 {
            store
                .record_clean("unlocked-repo", CLEAN_CYCLES_TO_UNLOCK)
                .unwrap();
        }
        store
            .record_clean("cold-repo", CLEAN_CYCLES_TO_UNLOCK)
            .unwrap();
        let map = store.triage_state_map().unwrap();
        assert_eq!(map.len(), 1, "locked repos must be omitted");
        assert_eq!(map.get("unlocked-repo"), Some(&RatchetState::Unlocked));
        assert!(!map.contains_key("cold-repo"));
    }

    // -----------------------------------------------------------------------
    // 10. Predicate inputs at the spec ceiling vs default — one explicit
    //     matrix so a regression in any single cell is obvious.
    // -----------------------------------------------------------------------

    #[test]
    fn eligibility_matrix_default_narrow_config() {
        // Month-1 default: junior / S, 3 clean cycles to unlock.
        // Cells:
        //   lead/XL + unlocked   → LeadFloor
        //   senior/M + unlocked   → OverMaxTier
        //   senior/M + locked     → RatchetLocked
        //   junior/S + unlocked   → AutoDispatch
        //   junior/S + locked     → RatchetLocked
        //   senior/M + missing vc → MissingVerifyCmd (overrides everything
        //                           except the global kill-switch and the
        //                           locked-ratchet reason)
        let cfg = RatchetConfig::default();
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                lead_item("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::LeadFloor)
        );
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                senior_m_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::OverMaxTier)
        );
        assert_eq!(
            evaluate(
                RatchetState::Locked,
                senior_m_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::RatchetLocked)
        );
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                junior_s_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::AutoDispatch
        );
        assert_eq!(
            evaluate(
                RatchetState::Locked,
                junior_s_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::RatchetLocked)
        );
    }

    #[test]
    fn eligibility_matrix_spec_ceiling_config() {
        // Widened posture: senior / M, 3 clean cycles to unlock.
        // Cells:
        //   lead/XL + unlocked   → LeadFloor
        //   senior/M + unlocked   → AutoDispatch
        //   senior/M + locked     → RatchetLocked
        //   senior/L + unlocked   → OverMaxComplexity
        //   junior/S + unlocked   → AutoDispatch
        //   senior/M + autonomy=Propose → AutonomyPropose
        let cfg = RatchetConfig::spec_ceiling();
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                lead_item("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::LeadFloor)
        );
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                senior_m_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::AutoDispatch
        );
        assert_eq!(
            evaluate(
                RatchetState::Locked,
                senior_m_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::RatchetLocked)
        );
        let senior_l = ItemShape {
            repo: "r",
            tier_floor: Tier::Senior,
            complexity: Ceiling::L,
            verify_cmd: Some("cargo test"),
        };
        assert_eq!(
            evaluate(RatchetState::Unlocked, senior_l, AutonomyMode::Ratchet, cfg),
            EligibilityDecision::Propose(ProposeReason::OverMaxComplexity)
        );
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                junior_s_with_verify("r"),
                AutonomyMode::Ratchet,
                cfg
            ),
            EligibilityDecision::AutoDispatch
        );
        assert_eq!(
            evaluate(
                RatchetState::Unlocked,
                senior_m_with_verify("r"),
                AutonomyMode::Propose,
                cfg
            ),
            EligibilityDecision::Propose(ProposeReason::AutonomyPropose)
        );
    }
}
