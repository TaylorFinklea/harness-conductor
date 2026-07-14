# Adversarial Design Review Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an isolated approval-gated workflow that runs N provider-diverse design reviewers plus one Lead synthesis judge over an immutable artifact snapshot.

**Architecture:** A new `adversarial` module owns artifact validation, immutable plan/state, pure panel selection, bounded read-only execution, schema parsing/repair, synthesis, and lifecycle results. It reuses only typed roster, Bursar decisions, backend argv construction, deck primitives, and ledger append; it never calls cycle, Arena, bd, git, worktree, Verify, or apply paths. CLI parsing remains thin and delegates all policy to the module.

**Tech Stack:** Rust 2024, serde/serde_json, chrono, sha2 from the bounded-approval plan, std threads for bounded parallelism, existing dispatch/deck/ledger primitives.

## Global Constraints

- `N` means N independent reviewers plus one additional judge call; `1 <= N <= 7` by default.
- Reviewers are Senior/Lead and use N distinct normalized provider keys.
- Judge is Lead, excluded by exact model from reviewer slots, and may share a provider.
- Artifact is a regular non-symlink file, at most 1 MiB, outside every `ai-scratch/` path component.
- Every reviewer receives byte-identical content/question/schema/instructions with no model identity.
- One same-model repair retry is allowed for malformed output; provider substitution is same-provider and preapproved only.
- Fewer than N valid reviews means no judge call and a partial/failed report.
- No bd, git, worktree, repository write, command execution from artifact text, or chezmoi apply.
- Dependency: provider-trust integration and the `sha2` plan contract must land first.
- Do not run mutating bare `cargo fmt`.

---

### Task 1: Parse config and snapshot immutable review plans

**Files:**
- Modify: `src/config.rs`
- Modify: `conductor.toml`
- Create: `src/adversarial.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces config table `[adversarial_review]` with `max_reviewers = 7`, bounded parallelism, judge roster name, and approved judge fallback names.
- Produces state root `~/.local/state/conductor/adversarial-reviews/<review-id>/` containing artifact snapshot, SHA-256, plan JSON, provider snapshot, and lifecycle state.

- [ ] **Step 1: Add red config and artifact-boundary tests.**

Cover defaults, max-reviewer range, unknown/non-Lead judge, invalid fallback names, directories, symlinks, >1 MiB files, unreadable files, and any path with an `ai-scratch` component. Assert the snapshot bytes and hash exactly match the accepted source.

- [ ] **Step 2: Run focused tests red.**

Run: `cargo test config::tests::adversarial_ adversarial::tests::artifact_`

Expected: compilation failure because the config/module do not exist.

- [ ] **Step 3: Implement config and immutable artifact plan scaffolding.**

Mirror `ArenaConfig` parsing for strict unknown-key and closed-roster validation, but use roster entry names rather than Arena profiles. Use `symlink_metadata` before reading, canonicalize only after rejecting symlinks, enforce byte size from metadata and read length, and write state atomically under the injected state root used by tests.

- [ ] **Step 4: Run focused tests.**

Run: `cargo test config::tests::adversarial_ adversarial::tests::artifact_`

Expected: all focused tests pass.

- [ ] **Step 5: Commit plan scaffolding.**

Run: `git add src/config.rs conductor.toml src/adversarial.rs src/main.rs`

Run: `git commit -m "feat: scaffold adversarial review plans"`

### Task 2: Select a provider-diverse approved panel

**Files:**
- Modify: `src/adversarial.rs`
- Modify: `src/bursar.rs`

**Interfaces:**
- Consumes: closed roster and one fresh Bursar snapshot.
- Produces: N reviewer slots with N normalized providers, one Lead judge, two alternatives per slot when available, approved same-provider fallbacks, and complete candidate audit/exclusion records.

- [ ] **Step 1: Add red table-driven panel tests.**

Assert exhausted/unknown exclusion, healthy before caution, maximum provider diversity, per-provider cost then tier then efficiency then roster ordering, exact N validation for `--models`, shortfall failure instead of duplicate provider, judge exact-model exclusion, and judge fallback validation.

- [ ] **Step 2: Run the selection tests red.**

Run: `cargo test adversarial::tests::panel_`

Expected: failure because no panel planner exists.

- [ ] **Step 3: Implement one pure selection function.**

Normalize provider keys from the roster `provider` field, not dispatch IDs. Keep reviewer and judge selection deterministic and return audit entries for every candidate. Explicit models constrain selection but never bypass tier, distinctness, provider state, or closed-roster checks.

- [ ] **Step 4: Run selection and Bursar tests.**

Run: `cargo test adversarial::tests::panel_ bursar::tests`

Expected: all tests pass.

- [ ] **Step 5: Commit panel planning.**

Run: `git add src/adversarial.rs src/bursar.rs`

Run: `git commit -m "feat: plan provider-diverse review panels"`

### Task 3: Publish and validate the immutable approval envelope

**Files:**
- Modify: `src/adversarial.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Produces an awaiting-approval harness-deck report with artifact hash, question, N+1 nominal calls, 2N+1 worst-case calls, reviewer/judge envelopes, alternatives, provider evidence, cost, exclusions, parallelism, and retry limits.
- Consumes approval only from the persisted report/run ID and exact approval block watermark.

- [ ] **Step 1: Add red plan/report/approval tests.**

Assert approval pins artifact hash, roster fingerprint, selected models, allowed fallbacks, judge, budgets, and call limits. A changed artifact/roster or missing/changes-requested approval must prevent execution. Validate the generated report with the injected deck validator.

- [ ] **Step 2: Run the focused tests red.**

Run: `cargo test adversarial::tests::approval_ deck::tests`

Expected: failure because no adversarial report or gate exists.

- [ ] **Step 3: Reuse deck primitives without cycle semantics.**

Create a dedicated report builder/block ID and lifecycle status; do not call `run_dry_run`, `run_dispatch_cycle`, or Arena report builders. Persist the exact plan before publishing, and compare current artifact/roster/provider routes to the approved envelope before any spawn.

- [ ] **Step 4: Run focused tests.**

Run: `cargo test adversarial::tests::approval_ deck::tests`

Expected: all tests pass.

- [ ] **Step 5: Commit the approval envelope.**

Run: `git add src/adversarial.rs src/deck.rs`

Run: `git commit -m "feat: approve immutable adversarial panels"`

### Task 4: Run reviewers read-only with schema repair

**Files:**
- Modify: `src/adversarial.rs`
- Modify: `src/dispatch.rs`

**Interfaces:**
- Reviewer schema: verdict `go | conditional-go | no-go`; findings with local ID/severity/claim/evidence/consequence/recommendation; assumptions; scope-to-cut; recommended sequencing.
- Produces: bounded parallel attempts, per-slot stdout/stderr logs, validated review JSON, and at most one same-model repair retry.

- [ ] **Step 1: Add red prompt, argv, parser, and parallelism tests.**

Assert byte-identical prompts across reviewers, no provider/model identity in prompt, artifact fencing as untrusted data, tools disabled/strongest read-only argv for every backend, malformed JSON repaired once, second malformed output failing the slot, same-provider-only fallback, and concurrency never exceeding config.

- [ ] **Step 2: Run the focused tests red.**

Run: `cargo test adversarial::tests::reviewer_ adversarial::tests::parallel_ dispatch::tests::adversarial_`

Expected: failures because the read-only execution path does not exist.

- [ ] **Step 3: Add a separate read-only argv constructor and bounded runner.**

Do not weaken normal worker argv. Build backend-specific read-only commands from the existing `argv_for_backend` patterns, close stdin, capture files under the review state root, validate extracted JSON, and run initial calls through bounded std threads. Repair prompts contain only the invalid output plus the same schema/instructions; they cannot change model or scope.

- [ ] **Step 4: Run reviewer and dispatch tests.**

Run: `cargo test adversarial::tests::reviewer_ adversarial::tests::parallel_ dispatch::tests::adversarial_`

Expected: all focused tests pass.

- [ ] **Step 5: Commit reviewer execution.**

Run: `git add src/adversarial.rs src/dispatch.rs`

Run: `git commit -m "feat: run bounded read-only design reviews"`

### Task 5: Synthesize anonymously and record every attempt

**Files:**
- Modify: `src/adversarial.rs`
- Modify: `src/ledger.rs`
- Modify: `src/deck.rs`

**Interfaces:**
- Judge schema: verdict, consensus, disagreements, unique risks, required changes, deferred questions, confidence, and coverage containing every `R1..RN` exactly once.
- Produces: anonymous deterministic review ordering, partial result without synthesis unless all N reviews validate, judge provider recheck, model-bench rows with roles `adversarial-reviewer` and `adversarial-judge`, and final report blocks.

- [ ] **Step 1: Add red synthesis, failure, ledger, and report tests.**

Cover missing one review suppressing judge spawn, anonymous deterministic IDs, minority positions preserved, duplicate/missing coverage rejected, judge provider becoming unavailable, approved judge fallback, all attempts logged including repair/failure, and final report containing individual reviews plus synthesis.

- [ ] **Step 2: Run focused tests red.**

Run: `cargo test adversarial::tests::judge_ adversarial::tests::partial_ ledger::tests::adversarial_`

Expected: failures because judge/ledger/report completion does not exist.

- [ ] **Step 3: Implement judge and lifecycle completion.**

Sort reviewer results by persisted slot, relabel only as `R1..RN`, recheck the approved judge chain immediately before spawn, validate exact coverage, append structured ledger rows for every attempt, and publish `done` or `partial` without fabricating synthesis.

- [ ] **Step 4: Run focused and full suites.**

Run: `cargo test adversarial::tests ledger::tests deck::tests`

Expected: all focused tests pass.

Run: `cargo test`

Expected: full suite passes.

- [ ] **Step 5: Commit synthesis and evidence.**

Run: `git add src/adversarial.rs src/ledger.rs src/deck.rs`

Run: `git commit -m "feat: synthesize and report adversarial reviews"`

### Task 6: Wire the CLI and prove the mutation boundary

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `README.md`
- Modify: `.docs/ai/current-state.md`
- Modify: `.docs/ai/roadmap.md`
- Create: `.docs/ai/phases/adversarial-design-review-report.md`

**Interfaces:**
- Produces exact commands from the spec: `conductor adversarial-review plan ...` and `conductor adversarial-review dispatch <review-id> ...`.

- [ ] **Step 1: Add red CLI grammar and mutation-sentinel tests.**

Cover required flags, N bounds, exactly N explicit models, plan output/report path, missing approval, successful dispatch, and sentinels proving no BdClient, git, worktree, cycle, repository write, or apply function is called.

- [ ] **Step 2: Run focused tests red.**

Run: `cargo test cli::tests::adversarial_ adversarial::tests::no_mutation_`

Expected: failure because CLI routing is absent.

- [ ] **Step 3: Add thin CLI dispatch.**

Keep manual parsing local to `cli.rs`, load config once, inject state/reports/ledger roots, and delegate policy to `adversarial`. Exit 2 for usage/config, 1 for failed/partial execution, and 0 only for a complete validated synthesis.

- [ ] **Step 4: Document the boundary and run every gate.**

Run: `cargo test`

Expected: all tests pass.

Run: `cargo clippy --all-targets -- -D warnings`

Expected: no new diagnostics in edited files.

Run: `cargo fmt -- --check`

Expected: exit 0; do not run mutating bare `cargo fmt`.

Run: `git diff --check`

Expected: exit 0.

- [ ] **Step 5: Update roadmap/report and commit.**

Document nominal/worst-case calls, state/report paths, schema contracts, provider shortfall behavior, and the no-mutation proof. Mark only the adversarial-review portion complete.

Run: `git add src/cli.rs src/main.rs README.md .docs/ai/current-state.md .docs/ai/roadmap.md .docs/ai/phases/adversarial-design-review-report.md`

Run: `git commit -m "docs: ship adversarial design review"`
