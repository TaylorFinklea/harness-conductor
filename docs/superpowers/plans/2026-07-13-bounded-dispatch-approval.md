# Bounded Dispatch Approval Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist the exact cycle approval scope so an approval can never launch more repositories or beads than the report displayed.

**Architecture:** A typed scope value is parsed by the cycle CLI, applied during scan selection, serialized into `CyclePlan`, and rendered in the approval report. Each planned item carries a SHA-256 hash over the routing/verification inputs that authorize launch. Dispatch consumes only persisted scope and refuses widened, missing, or changed inputs.

**Tech Stack:** Rust 2024, serde/serde_json, sha2 0.10, existing scan/plan/deck/dispatch modules.

## Global Constraints

- Unscoped approval dispatches existing `dispatches` only; proposals remain inert.
- Explicit repo/item scope may dispatch visible proposals only inside that immutable scope.
- Dispatch accepts no selectors and cannot widen or replace plan scope.
- Unknown, duplicate, excluded, incompatible, or empty selectors are usage/config errors.
- Changed item inputs require a replan; no substitute item may be introduced.
- Do not run mutating bare `cargo fmt`.

---

### Task 1: Define immutable scope and item-input hashes

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `.docs/ai/decisions.md`
- Modify: `src/plan.rs`

**Interfaces:**
- Produces: scope kinds `fleet-audit | repository-scope | exact-item-scope`, canonical selectors, canonical repo paths, maximum dispatch count, and per-item SHA-256 input hashes.
- Hash inputs: repo canonical path, issue ID, title/description/acceptance, routing metadata, Verify command, selected model, and approved fallback envelope.

- [ ] **Step 1: Add red serialization and hash-stability tests.**

Assert canonical ordering makes selector order irrelevant, duplicate selectors are rejected before serialization, one relevant input change alters the hash, and unrelated issue fields outside the authorization set do not.

- [ ] **Step 2: Run plan tests red.**

Run: `cargo test plan::tests`

Expected: failure because scope and hashes do not exist.

- [ ] **Step 3: Add SHA-256 with an ADR and implement the plan types.**

Record why `sha2 = "0.10"` is used instead of process-dependent standard hashing or shelling out to `shasum`. Keep hashing deterministic by serializing an internal ordered input structure, not a `HashMap`.

- [ ] **Step 4: Run plan tests.**

Run: `cargo test plan::tests`

Expected: all plan tests pass.

- [ ] **Step 5: Commit the immutable plan contract.**

Run: `git add Cargo.toml Cargo.lock .docs/ai/decisions.md src/plan.rs`

Run: `git commit -m "feat: persist bounded dispatch scope"`

### Task 2: Parse and enforce cycle selectors

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/cycle.rs`
- Modify: `src/scan.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Produces CLI: `conductor cycle --dry-run [--repo <name|path>]... [--only <repo>:<issue-id>]... [--config <path>]`.
- Consumes: the typed scope and hash builder from Task 1.

- [ ] **Step 1: Add red CLI/selection tests.**

Cover multiple repo and only selectors, `--only` satisfying `--repo`, unknown repo/issue, duplicate selector, hard-excluded repo, incompatible selector, and empty result. Add the 103-proposal fleet regression fixture as an unscoped plan.

- [ ] **Step 2: Run focused tests red.**

Run: `cargo test cli::tests::cycle_scope_ cycle::tests::scope_`

Expected: failures because current cycle grammar accepts only config and dry-run.

- [ ] **Step 3: Apply scope before plan persistence.**

Resolve names/paths against actual scan snapshots, canonicalize repository paths, fail before report publication on selector errors, compute item hashes after triage selection, and render scope kind/selectors/max dispatch count in the approval block.

- [ ] **Step 4: Run cycle and deck tests.**

Run: `cargo test cli::tests::cycle_scope_ cycle::tests deck::tests`

Expected: all focused tests pass.

- [ ] **Step 5: Commit scoped planning.**

Run: `git add src/cli.rs src/cycle.rs src/scan.rs src/deck.rs`

Run: `git commit -m "feat: scope cycle approval reports"`

### Task 3: Make dispatch consume only persisted scope

**Files:**
- Modify: `src/dispatch_cycle.rs`
- Modify: `src/plan.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Consumes: persisted scope and hashes; no dispatch-time selector input.
- Produces: exact semantics—unscoped `dispatches` only, explicit-scope eligible visible entries, and replan-required skips on changed inputs.

- [ ] **Step 1: Rewrite the approval-gate regression matrix red.**

Add tests for bare unscoped approval launching zero of 103 proposals, exact-item scope launching only chosen IDs, repository scope launching no other repo, changed hash preventing claim/spawn, and dispatch API/CLI rejecting any attempted scope widening.

- [ ] **Step 2: Run focused tests red.**

Run: `cargo test dispatch_cycle::tests::approval_ dispatch_cycle::tests::scope_`

Expected: the current `planned_items` behavior launches proposals and fails the regressions.

- [ ] **Step 3: Replace combined-bucket planning with scope-aware selection.**

Keep existing automatic `dispatches` behavior within budgets. Re-read each issue before claim, rebuild its authorization hash, and report changed/disappeared/ineligible items without substituting. Do not add selectors to `conductor dispatch`.

- [ ] **Step 4: Run dispatch and full suites.**

Run: `cargo test dispatch_cycle::tests`

Expected: all dispatch-cycle tests pass.

Run: `cargo test`

Expected: all tests pass.

- [ ] **Step 5: Commit bounded dispatch.**

Run: `git add src/dispatch_cycle.rs src/plan.rs src/deck.rs`

Run: `git commit -m "fix: bind approval to persisted dispatch scope"`

### Task 4: Close conductor-xa5 and document the blast-radius guarantee

**Files:**
- Modify: `.docs/ai/current-state.md`
- Modify: `.docs/ai/roadmap.md`
- Create: `.docs/ai/phases/bounded-dispatch-approval-report.md`

- [ ] **Step 1: Run final verification.**

Run: `cargo test`

Expected: all tests pass, including the 103-proposal regression.

Run: `cargo clippy --all-targets -- -D warnings`

Expected: no new diagnostics in edited files.

Run: `cargo fmt -- --check`

Expected: exit 0; do not mutate unrelated baseline formatting.

Run: `git diff --check`

Expected: exit 0.

- [ ] **Step 2: Close `conductor-xa5` with the regression evidence.**

Run: `bd close conductor-xa5 --reason "Approval scope is persisted; unscoped approval leaves proposals inert; exact/repo scope and item hashes are regression-tested."`

- [ ] **Step 3: Update report and handoff docs.**

State the maximum blast-radius rule, test fixture, and that dispatch still performs normal claim/verify only after scoped approval.

- [ ] **Step 4: Commit documentation.**

Run: `git add .docs/ai/current-state.md .docs/ai/roadmap.md .docs/ai/phases/bounded-dispatch-approval-report.md`

Run: `git commit -m "docs: close bounded dispatch approval"`
