# Adversarial design review report

Status: complete, 2026-07-15. Beads: `conductor-i8r`, `conductor-0zv`,
`conductor-b35`, `conductor-2cr`, `conductor-vly`, `conductor-j84`.

## Shipped

- Exact `adversarial-review plan` / `dispatch <review-id>` CLI.
- Immutable artifact, roster, provider, panel, call-limit, report, and approval
  envelope; changed inputs require a new plan.
- `N` Senior/Lead reviewers on distinct eligible providers; closed-roster,
  deterministic alternatives and approved same-provider fallback only.
- Bounded tools-disabled execution; one schema-repair attempt per reviewer;
  nominal `N + 1`, worst-case `2N + 1` calls.
- Additional Lead judge only for a complete panel; fresh judge-chain provider
  recheck; anonymous deterministic `R1..RN` input.
- Strict reviewer/judge schemas; exact coverage once per anonymous ID;
  disagreements and minority positions preserved.
- Structured attempt ledger roles `adversarial-reviewer` and
  `adversarial-judge`, including repair, failure, fallback, and judge attempts.
- Terminal report with anonymous individual reviews, valid synthesis,
  disagreements, failures, and complete/partial outcome.

## Persistence

- State: `~/.local/state/conductor/adversarial-reviews/<review-id>/`.
- Report: `~/.harness/reports/conductor/<review-id>/report.json`.
- Ledger: `~/.claude/model-bench.jsonl`.
- Schemas: `conductor-adversarial-plan-v1`,
  `conductor-adversarial-provider-snapshot-v1`,
  `conductor-adversarial-lifecycle-v1`, `harness-deck/report@1`.
- Root injection: `CONDUCTOR_STATE_DIR`, `CONDUCTOR_REPORTS_HOME`,
  `CONDUCTOR_LEDGER_PATH`.

## Failure posture

- Provider/model shortfall: plan fails; never duplicates a provider.
- Missing/changed approval or immutable input: dispatch refuses before spawn.
- Missing/failed reviewer: partial result, no judge, no fabricated synthesis.
- Ineligible judge chain: partial result, no synthesis.
- Invalid or incomplete judge coverage: partial result, no synthesis.
- Usage/configuration exit `2`; partial/failed dispatch exit `1`; complete,
  schema-valid dispatch exit `0`.

## Mutation proof

- No Beads, Git, worktree, normal-cycle dispatch, repository-write, or apply
  seam in the adversarial runtime.
- Artifact/question/model outputs treated as untrusted data.
- Fake executor tests keep Beads/Git/worktree/cycle/repository/chezmoi
  sentinels byte-identical and place every process cwd under review state.
- Live/metered model dispatch was not used for tests.

## Verification

- Focused CLI grammar/approval/no-mutation tests pass.
- `cargo test`: 316 unit + 1 integration pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo fmt -- --check`: pass.
- `git diff --check`: pass.

## Boundary

- Existing cycle, Arena, roster, route, and qualitative-review behavior kept.
- Later Conductor-core consolidation and migration into a `review` job not
  started; preserve this behavior for that separate Guildhall session.
