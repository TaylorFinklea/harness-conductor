# Provider Trust Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Bursar status@2 a fail-closed planning and dispatch input, with explainable route advice and runtime 429 writeback before fallback.

**Architecture:** `bursar.rs` owns the v2 subprocess contract, pure action mapping, and sanitized writeback request. Triage consumes typed provider decisions after existing eligibility gates and before cost/efficiency ordering. `CyclePlan` persists approval-time provider evidence, while dispatch reuses the same evaluator for fresh rechecks. `route explain` calls the pure selector without scan, bd, claim, or dispatch mutation.

**Tech Stack:** Rust 2024, serde/serde_json, chrono, existing subprocess/triage/deck/ledger seams.

## Global Constraints

- Consume only `bursar/status@2`; status@1, malformed, missing, absent-provider, stale, and command failures defer when enabled.
- `budgets.use_bursar=false` is the only static-caps override and is visibly labeled.
- Provider state cannot relax tier, ceiling, data-policy, scope, Verify, or closed-roster gates.
- Runtime raw stderr never crosses the Bursar writeback boundary.
- Writeback happens before an approved fallback attempt.
- Do not run bare `cargo fmt`; scope formatting to edited files or use `cargo fmt -- --check`.

---

### Task 1: Parse status@2 and expose typed provider decisions

**Files:**
- Modify: `src/bursar.rs`
- Modify: `src/config.rs`
- Modify: `conductor.toml`

**Interfaces:**
- Produces: v2 availability values and a provider decision record containing provider/model, availability/source/freshness/expiry, action, reason, and expiry basis.
- Produces: `budgets.unknown_429_cooldown` with a default of 15 minutes.
- Produces: a sanitized runtime-observation request through the existing `BursarClient` test seam.

- [ ] **Step 1: Rewrite the Bursar unit matrix as red status@2 tests.**

Cover all four availability values, missing binary, bad command, malformed JSON, status@1, absent provider, stale evidence, and explicit disabled mode. Assert exact actions: healthy proceed, caution spend-cautiously, everything else defer unless disabled.

- [ ] **Step 2: Run the focused tests red.**

Run: `cargo test bursar::tests`

Expected: failures because the parser still expects status@1 and unknown/unavailable are not uniformly deferred.

- [ ] **Step 3: Implement v2 parsing and config validation.**

Mirror the existing `CommandBursarClient` subprocess capture and `FakeBursarClient`. Remove percentage inference from Conductor; availability is authoritative. Add `unknown_429_cooldown` to the existing `[budgets]` parser, default, checked-in config fixture, and invalid-config tests. Keep static caps only behind the explicit false setting.

- [ ] **Step 4: Run focused and config gates.**

Run: `cargo test bursar::tests config::tests`

Expected: all focused tests pass.

- [ ] **Step 5: Commit the contract consumer.**

Run: `git add src/bursar.rs src/config.rs conductor.toml`

Run: `git commit -m "feat: consume bursar availability v2"`

### Task 2: Add provider-aware pure routing and read-only route explain

**Files:**
- Modify: `src/triage.rs`
- Create: `src/route.rs`
- Modify: `src/main.rs`
- Modify: `src/cli.rs`

**Interfaces:**
- Consumes: existing `candidate_rejection`, routing fields, repo cost policy, roster order, and typed provider decisions.
- Produces: one pure selection/audit result shared by cycle planning and `route explain`.
- Produces CLI: `conductor route explain --repo <path> --tier-floor <...> --complexity <...> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]`.

- [ ] **Step 1: Add red table tests for provider ordering and exclusions.**

Assert existing eligibility gates run first, exhausted/unknown are excluded, healthy outranks caution, configured fallback remains within eligibility, two alternatives are returned when available, and full candidate audit entries explain every exclusion.

- [ ] **Step 2: Add red CLI tests proving route explain is read-only.**

Use a temporary config/Bursar fake and assert JSON/human output. The test must demonstrate no scan, `bd`, state-plan write, claim, or dispatch call.

- [ ] **Step 3: Run focused tests red.**

Run: `cargo test triage::tests route::tests cli::tests::route_`

Expected: compilation failure because `route` and CLI grammar do not exist.

- [ ] **Step 4: Implement the shared pure selector and thin CLI.**

Keep existing routing-field parsing untouched. Build the candidate audit from actual closed-roster entries, pass in one Bursar snapshot, and let hard-excluded repos receive advice without changing the dispatch exclusion. Do not duplicate ordering in `cli.rs`.

- [ ] **Step 5: Run focused and full tests.**

Run: `cargo test triage::tests route::tests cli::tests::route_`

Expected: all focused tests pass.

Run: `cargo test`

Expected: all tests pass.

- [ ] **Step 6: Commit route advice.**

Run: `git add src/triage.rs src/route.rs src/main.rs src/cli.rs`

Run: `git commit -m "feat: explain provider-aware routes"`

### Task 3: Persist provider evidence during cycle planning

**Files:**
- Modify: `src/plan.rs`
- Modify: `src/cycle.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Consumes: the pure route audit from Task 2 and one Bursar snapshot per dry run.
- Produces: provider decisions embedded in the immutable cycle plan and rendered in the same report used for approval.

- [ ] **Step 1: Add red plan serialization and dry-run tests.**

Assert every metered candidate carries provider/model, availability/source, checked/data-as-of/expiry, action/reason, fallback/defer result, and expiry basis. Assert unknown/exhausted candidates never enter dispatchable selection and the complete chain remains visible.

- [ ] **Step 2: Run the focused tests red.**

Run: `cargo test plan::tests cycle::tests`

Expected: failures because `CyclePlan` has no provider-decision field and `cycle` does not query Bursar.

- [ ] **Step 3: Thread one snapshot through dry-run planning.**

Follow `run_dry_run_with_timestamps` and the injected client patterns; do not shell out per candidate. Render the same typed records into the approval report candidate audit. If Bursar is enabled and the snapshot cannot be trusted, persist terminal defers rather than silently dropping candidates.

- [ ] **Step 4: Run plan/cycle and full gates.**

Run: `cargo test plan::tests cycle::tests`

Expected: focused tests pass.

Run: `cargo test`

Expected: full suite passes.

- [ ] **Step 5: Commit approval-time provider evidence.**

Run: `git add src/plan.rs src/cycle.rs src/deck.rs`

Run: `git commit -m "feat: persist provider decisions in cycle plans"`

### Task 4: Recheck approved routes and write runtime failures back

**Files:**
- Modify: `src/dispatch_cycle.rs`
- Modify: `src/bursar.rs`
- Modify: `src/ledger.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Consumes: persisted approved provider/model/fallback envelope and fresh Bursar status.
- Produces: fail-closed pre-attempt rechecks and sanitized `observe` writeback before fallback.

- [ ] **Step 1: Add red dispatch regression tests.**

Cover a route becoming exhausted after approval, a newly healthy but unapproved provider remaining forbidden, a parsed reset labeled `provider-reset`, an absent reset using the configured 15-minute `local-cooldown`, writeback occurring before fallback, writeback failure remaining visible/fail-closed, and raw stderr never reaching the fake Bursar client.

- [ ] **Step 2: Run the focused tests red.**

Run: `cargo test dispatch_cycle::tests::bursar_ dispatch_cycle::tests::fallback_`

Expected: failures show no approved-envelope recheck and no observation writeback.

- [ ] **Step 3: Implement recheck and writeback in the existing worker chain.**

Reuse `retryable_failure_reason`; add a narrow reset extractor for known trustworthy timestamps, otherwise compute the configured cooldown. Record attempt/classification first, call the typed Bursar observation method, then consider only fallbacks already present in the persisted envelope. A writeback command failure adds a report/ledger warning but never re-enables the failed provider.

- [ ] **Step 4: Run dispatch and full gates.**

Run: `cargo test dispatch_cycle::tests`

Expected: all dispatch-cycle tests pass.

Run: `cargo test`

Expected: full suite passes.

- [ ] **Step 5: Commit runtime trust enforcement.**

Run: `git add src/dispatch_cycle.rs src/bursar.rs src/ledger.rs src/deck.rs`

Run: `git commit -m "feat: recheck and record provider failures"`

### Task 5: Verify the integrated provider-trust slice

**Files:**
- Modify: `.docs/ai/current-state.md`
- Modify: `.docs/ai/roadmap.md`
- Create: `.docs/ai/phases/provider-trust-integration-report.md`

- [ ] **Step 1: Run all non-mutating gates.**

Run: `cargo test`

Expected: all tests pass.

Run: `cargo clippy --all-targets -- -D warnings`

Expected: exit 0 or only explicitly documented pre-existing diagnostics; no new diagnostic in edited files.

Run: `cargo fmt -- --check`

Expected: exit 0; do not run mutating bare `cargo fmt`.

Run: `git diff --check`

Expected: exit 0.

- [ ] **Step 2: Record test evidence and rollout dependency.**

The report must identify the installed-pair requirement with Bursar v2, exact route-explain smoke commands, and the fact that no live provider dispatch was required for automated verification.

- [ ] **Step 3: Update handoff state without closing unrelated roadmap work.**

Mark only the provider-trust portion complete; bounded approval and adversarial review remain independently tracked until their plans finish.

- [ ] **Step 4: Commit handoff documentation.**

Run: `git add .docs/ai/current-state.md .docs/ai/roadmap.md .docs/ai/phases/provider-trust-integration-report.md`

Run: `git commit -m "docs: report provider trust integration"`
