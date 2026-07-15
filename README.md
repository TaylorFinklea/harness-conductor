# Conductor

Conductor is a Rust orchestration binary for bounded fleet cycles and isolated,
approval-gated adversarial design review.

## Adversarial design review

Plan a review over one immutable artifact, approve the published harness-deck
report, then dispatch the exact approved review ID:

```text
conductor adversarial-review plan --artifact <path> --reviewers <N> [--question <text>] [--models <a,b,...>] [--config <path>]
conductor adversarial-review dispatch <review-id> [--config <path>]
```

`N` must be between 1 and `adversarial_review.max_reviewers` (7 by
default). `--models` must contain exactly `N` closed-roster names. Reviewers
must be Senior or Lead and use distinct normalized providers. Explicit models
cannot bypass provider eligibility, tier, roster, or provider-diversity gates.
If the eligible provider set is too small, planning fails before artifact state
is written.

The nominal call count is `N + 1`: one call per reviewer and one additional
Lead judge. The worst case is `2N + 1`, allowing one same-model schema-repair
retry for each reviewer. Provider failure may use only a same-provider fallback
recorded in the approved reviewer chain. The judge may use only its configured,
approved Lead fallback chain and is rechecked immediately before it starts.

Planning snapshots and hashes the artifact, roster, provider evidence, panel,
limits, question, and approval watermark. Dispatch requires the exact
plan-bound approval response. A changed artifact, roster, report, sidecar,
provider route, or approval envelope fails closed and requires a new plan.

Dispatch exit codes are:

- `0`: every reviewer output and the anonymous judge synthesis passed schema
  validation, including exact `R1..RN` coverage.
- `1`: approval is absent, execution failed, the panel is partial, or synthesis
  is missing or invalid.
- `2`: command usage or configuration is invalid.

A successful `plan` command exits `0`; planning failures exit `1`, while its
usage and configuration failures exit `2`.

### State, reports, ledger, and schemas

The default roots are:

- state: `~/.local/state/conductor/adversarial-reviews/<review-id>/`
- report: `~/.harness/reports/conductor/<review-id>/report.json`
- ledger: `~/.claude/model-bench.jsonl`

`CONDUCTOR_STATE_DIR`, `CONDUCTOR_REPORTS_HOME`, and
`CONDUCTOR_LEDGER_PATH` inject alternate roots. The state directory contains
the exact artifact bytes and hash, `plan.json`, `provider-snapshot.json`,
`lifecycle.json`, reviewer attempt logs and validated `review.json` files, and
judge logs.

Persisted contracts are `conductor-adversarial-plan-v1`,
`conductor-adversarial-provider-snapshot-v1`, and
`conductor-adversarial-lifecycle-v1`; reports use `harness-deck/report@1`.
Reviewer JSON contains verdict, findings, assumptions, scope to cut, and
recommended sequencing. Judge JSON contains verdict, consensus,
disagreements, unique anonymous risks, required changes, deferred questions,
confidence, and coverage. Unknown fields fail validation, and coverage must
contain each anonymous reviewer ID exactly once. Ledger attempts use roles
`adversarial-reviewer` and `adversarial-judge`.

### Mutation boundary

Artifact contents, questions, reviewer output, and judge output are untrusted
data. Model processes receive tools-disabled/read-only argv, null stdin, and a
working directory inside adversarial state. This path never invokes Beads,
Git, worktree management, normal cycle dispatch, target-repository writes, or
`chezmoi apply`. Its only writes are the injected state, harness-deck report,
and ledger paths. Tests inject fake execution and mutation sentinels; they do
not make live or metered reviewer calls.

This feature remains separate from normal cycle, Arena, and qualitative review
jobs. It does not implement the later Conductor-core consolidation or migrate
adversarial review into a `review` job.
