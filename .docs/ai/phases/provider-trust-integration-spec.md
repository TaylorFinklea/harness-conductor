# Conductor provider-trust integration

Status: approved design, 2026-07-13. Depends on Bursar
`provider-availability-v2-spec.md`.

## Goal

Make provider availability part of planning and dispatch instead of a late
warning. Unknown or exhausted providers must not receive speculative work, and
runtime quota failures must be written back to the shared Bursar state before
fallback.

## Decision model

Conductor consumes only `bursar/status@2`:

| Bursar availability | Budget action |
|---|---|
| `healthy` | `proceed` |
| `caution` | `spend-cautiously`; rank behind a healthy equivalent |
| `exhausted` | `defer` |
| `unknown` | `defer` |

With `budgets.use_bursar = true`, a missing binary, command failure,
unsupported schema, missing provider, stale status, or malformed report maps
to `defer`. The existing `budgets.use_bursar = false` setting remains the
only explicit static-caps override and is always called out in plans/reports.

## Planning

Cycle dry-run and adversarial-review planning fetch one Bursar snapshot and
annotate every metered candidate with availability, freshness, source, and
reason. The candidate audit table explains selection and exclusion; it never
silently removes a candidate.

Provider state participates after tier/ceiling/data-policy eligibility and
before efficiency/cost ordering:

1. exhausted/unknown are ineligible
2. healthy ranks before caution
3. existing cost/efficiency/roster tie-breaks apply

Declared provider outlook remains explanatory policy and cannot promote
unknown/exhausted live state when Bursar is enabled.

If the current model is ineligible, planning may choose a configured fallback
that still satisfies routing and provider policy. If none exists, the item is
deferred with the complete chain explanation. All-cheap-lanes unavailable
halts that item; planning never silently crosses into an expensive provider
outside the visible chain.

## Dispatch recheck and approval stability

Immediately before each metered attempt, Conductor fetches fresh status. It may
continue only with a model/provider already present in the approved envelope.
A newly exhausted/unknown provider is skipped. A newly eligible provider not
in the envelope still requires reapproval.

Provider status cannot make an otherwise ineligible model eligible and cannot
change tier, ceiling, data-policy, repo, issue, or Verify scope.

## Runtime writeback

When the existing retryable classifier identifies 429/quota/rate-limit
failure:

1. record the attempt and sanitized classification
2. parse a trustworthy provider reset when present
3. otherwise apply `budgets.unknown_429_cooldown` (default 15 minutes), clearly
   labeled as `local-cooldown`
4. invoke Bursar's `observe` command before trying fallback
5. consider only already-approved eligible fallbacks

Raw stderr is never passed to Bursar. A writeback failure is a visible warning
and cannot reactivate the failed provider; fallback for the current chain may
continue. The next status failure remains fail-closed.

## Explainability

Dry-run, dispatch, and live reports use the same provider decision record:

- provider/model
- availability and source
- checked/data-as-of/expiry timestamps
- action and exact reason
- selected fallback or terminal defer
- whether expiry is provider reset, local cooldown, or human override

This record is persisted with the plan so approval-time and dispatch-time
states can be compared.

## Read-only route advisor

Non-beads front doors use Conductor without entering the fleet cycle:

```text
conductor route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]
```

It applies repo data policy, closed-roster gates, provider availability, and
the same ordering as planning. It returns the chosen route, two alternatives,
backend, dispatch ID, reasoning effort, provider evidence, and exclusions. It
is read-only and may advise for hard-excluded repos; exclusion still prevents
Conductor dispatch. Skills may translate the chosen backend/ID into an approved
Ralph or native-subagent launch but may not rerun routing themselves.

## Tests

- Every v2 availability maps to the exact action table.
- Missing Bursar, bad command, schema v1, absent provider, and malformed JSON
  all defer when enabled.
- Explicit `use_bursar=false` remains static caps and is visibly labeled.
- Planning excludes exhausted/unknown and prefers healthy over caution.
- Dispatch recheck rejects a route that became exhausted after approval.
- No unapproved provider enters the fallback chain after recheck.
- Parsed reset writes `provider-reset`; absent reset writes the configured
  15-minute `local-cooldown` without claiming it is provider telemetry.
- Bursar writeback occurs before fallback and receives no raw stderr.
- All provider decisions appear in the persisted plan and report.
- Route advisor output matches the same pure selection result as cycle
  planning and performs no scan, claim, or dispatch mutation.

## Non-goals

- Predictive quota modeling, automatic provider probing, or inferred resets.
- Replacing roster fallback configuration.
- Changing tier/ceiling/data-policy semantics.
- Implementing product-test milestone selection or adversarial review output.
