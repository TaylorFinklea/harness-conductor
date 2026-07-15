# Adversarial design review report

Status: complete, 2026-07-15. Beads: `conductor-i8r`, `conductor-0zv`,
`conductor-b35`, `conductor-2cr`, `conductor-vly`, `conductor-j84`,
`conductor-vcr`.

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
- Automated tests use injected fake execution. One explicitly authorized,
  disposable post-land E2E used live model execution as recorded below.

## Post-land review and live E2E

- Read-only Luna and GLM 5.2 reviews completed through `pi-liveness`; every
  finding was checked against the final tree before action.
- Fixed one confirmed crash-ordering defect: a schema-valid spawned judge is
  now ledgered before the fallible synthesis sidecar write. A forced sidecar
  publication failure proves the judge row survives.
- Expanded the static no-mutation seam guard and added a real fake-executor
  partial-panel CLI test proving exit `1` with no judge or synthesis.
- The checked-in production config failed closed at planning under its live
  provider snapshot when no approved reviewer/judge combination remained;
  no artifact state was created.
- A disposable static-cap plan used one Ollama Cloud GLM 5.2 Senior reviewer
  plus one distinct Terra Lead judge. The first dispatch correctly returned
  partial/exit `1` and ledgered both attempts when Codex rejected the isolated
  non-Git cwd.
- Added Codex `--skip-git-repo-check` only to the tools-disabled,
  `--sandbox read-only` adversarial argv. A fresh approved plan then returned
  exit `0`: one valid anonymous `R1` review, one valid synthesis with exact
  coverage, zero lifecycle failures, and exactly two structured ledger rows.
- `harness-deck validate` accepted the six-block terminal report. The source
  artifact SHA-256 stayed byte-identical, and redispatching the terminal ID
  returned exit `1` without adding ledger rows.

## Verification

- Focused CLI grammar/approval/no-mutation tests pass.
- `cargo test`: 318 unit + 1 integration pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo fmt -- --check`: pass.
- `git diff --check`: pass.

## Boundary

- Existing cycle, Arena, roster, route, and qualitative-review behavior kept.
- Later Conductor-core consolidation and migration into a `review` job not
  started; preserve this behavior for that separate Guildhall session.
