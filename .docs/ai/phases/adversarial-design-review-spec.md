# Conductor adversarial design review

Status: approved design, 2026-07-13. `N` means `N` independent reviewers plus
one additional synthesis/judge call.

## Mission

Add a bounded Conductor workflow for challenging a specification or
architectural decision with multiple provider-diverse models. Conductor owns
selection, provider preflight, approval, execution, schema validation,
logging, and the harness-deck report. A chezmoi-managed skill is only the
cross-harness front door.

This is separate from:

- normal `cycle` scan/claim/dispatch/verify work
- Arena's same-bead harness comparison and patch selection
- the shipped post-implementation qualitative-review stage
- Ralph phase loops

It performs no bd, git, worktree, repository, or apply mutation.

## CLI and lifecycle

```text
conductor adversarial-review plan --artifact <path> --reviewers <N> [--question <text>] [--models <a,b,...>] [--config <path>]
conductor adversarial-review dispatch <review-id> [--config <path>]
```

`1 <= N <= config.adversarial_review.max_reviewers`; default maximum is 7.
The plan shows nominal calls (`N + 1`) and worst-case calls (`2N + 1`, when
every reviewer needs its single repair retry). `--models` names exactly N
roster entries and never bypasses provider health, distinctness, or
closed-roster validation.

Plan:

1. Read and snapshot the artifact; reject directories, symlinks, files over
   1 MiB, unreadable files, and any path under `ai-scratch/`.
2. Hash the exact bytes and build the common reviewer prompt.
3. Read the closed roster and a fresh Bursar snapshot.
4. Select N reviewers on N distinct normalized provider keys.
5. Select one Lead judge, excluding the exact reviewer models. The judge may
   share a provider with a reviewer.
6. Persist the immutable plan and publish an awaiting-approval report showing
   chosen models, two alternatives per slot when available, provider state,
   cost, selection reason, exclusions, artifact hash, and limits.

Dispatch refuses unless that review ID is approved. It re-hashes the artifact,
re-reads the roster, and rechecks Bursar immediately before launch. A changed
artifact/roster or a route outside the approved model/fallback envelope
requires a new plan and approval.

## Selection policy

Reviewer candidates must be Senior or Lead roster entries. The pure planner:

1. excludes `exhausted` and `unknown` providers
2. prefers `healthy` over `caution`
3. maximizes distinct normalized providers
4. within each provider, prefers lower cost, then higher tier, then better
   efficiency, then roster order

The plan visibly labels the cheap-lane preference and every excluded
candidate. If fewer than N provider groups are eligible, planning stops with
the shortfall; it never fills two slots from one provider.

A bounded manual Bursar allow may make an otherwise opaque provider
`caution`; it never upgrades that provider to healthy or overrides active
exhaustion.

The judge must be Lead and healthy or caution, uses
`adversarial_review.judge` followed by its configured fallback list, and is
shown in the approval envelope. If no judge is eligible, planning stops.

An explicit `budgets.use_bursar = false` remains the operator's visible static
caps override. When Bursar is enabled, missing binary, unsupported schema,
absent provider, stale data, and source error all fail closed.

## Reviewer and judge contracts

Every reviewer receives byte-identical artifact content, question, output
schema, and instructions. Provider/model identity is not embedded in the
prompt. The artifact and other reviews are untrusted data: embedded
instructions cannot alter tools, scope, output location, or dispatch policy.

Reviewers run headlessly with tools disabled or the backend's strongest
read-only equivalent. The N initial calls run in bounded parallel. They return
structured output:

- verdict: `go | conditional-go | no-go`
- findings: stable local ID, severity, claim, evidence, consequence,
  recommendation
- assumptions
- scope to cut
- recommended sequencing

One repair retry is allowed for malformed output using the same approved
model. A provider failure may use only an approved same-provider fallback. A
different-provider substitution changes the review panel and requires a new
plan. Unless all N valid reviews exist, the judge does not run and the report
ends partial/failed.

The judge receives anonymous `R1..RN` reviews in a deterministic order and
returns:

- verdict: `go | conditional-go | no-go`
- consensus
- disagreements, preserving minority positions
- unique risks, attributed only to anonymous reviewer IDs
- required changes
- deferred questions
- confidence and coverage of all reviewer IDs

The judge may not silently discard a review. Schema validation requires every
reviewer ID exactly once in coverage. Provider state is rechecked before the
judge; if its approved provider is no longer eligible and no approved judge
fallback remains, the run ends partial without synthesis.

## State, reports, and logging

State root:
`~/.local/state/conductor/adversarial-reviews/<review-id>/`.

It contains the artifact snapshot, plan JSON, provider snapshot, reviewer
outputs/logs, judge output/logs, and lifecycle state. Report project remains
`conductor`; blocks include the approval envelope, candidate audit table,
provider callouts, individual reviews, and synthesis.

Every reviewer and judge attempt appends a `model-bench.jsonl` row with role
`adversarial-reviewer` or `adversarial-synthesis`, review ID, schema-valid
result, duration, provider/model, and failure reason when present. The
cross-harness skill appends one required prose Experience Log line per actual
reviewer, repair, and judge dispatch after assessing the outputs.

## Provider writeback

On a classified runtime 429/quota failure, Conductor records the current
attempt as failed, invokes Bursar's observation interface with a sanitized
reason and parsed provider reset, and then considers only approved fallbacks.
When no reset is present, it uses the configured local cooldown from the
provider-trust integration and labels it as routing policy rather than
provider telemetry. A writeback failure is visible and cannot make the
provider eligible.

## Test boundaries

Pure planner tests:

- N reviewers always use N distinct provider keys.
- healthy beats caution; exhausted/unknown never route.
- fewer than N healthy/caution providers fails with an explainable shortfall.
- explicit models cannot bypass closed roster, health, or distinctness.
- judge is Lead, is not an exact reviewer model, and is included in approval.
- selection and alternative ordering are deterministic.

Lifecycle tests with fake executors/Bursar/report sink:

- plan is read-only and snapshots/hash-pins exact artifact bytes.
- dispatch requires approval and rejects changed artifact, roster, or route.
- all reviewers receive byte-identical prompts and launch with no tools.
- malformed output gets one repair retry; remaining partial panels skip judge.
- judge receives anonymous reviews and covers R1..RN exactly once.
- runtime 429 writes an exhausted observation before approved fallback.
- no bd/git/worktree/apply method is called.
- N reviewer rows plus one judge row reach the ledger.

Regression tests prove existing `cycle`, Arena, and qualitative review output
and command parsing are unchanged.

## Non-goals

- Letting reviewers edit the artifact or repository.
- Multi-round debate, reviewer-to-reviewer chat, voting, or automatic spec
  rewriting.
- Treating multiple models on one provider as provider diversity.
- Unattended execution without an approved immutable plan.
