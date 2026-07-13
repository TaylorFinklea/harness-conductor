# Bounded dispatch approval

Status: approved design, 2026-07-13. Resolves `conductor-xa5` and is required by
the product-test readiness front door.

## Problem

The current cycle approval is binary and `run_dispatch_cycle` combines
`plan.dispatches` with every `plan.proposals` item. An approval over a fleet
scan can therefore launch all proposals, even when the operator intended one
bead or repository.

## Scope contract

Cycle dry-run accepts an optional explicit scope:

```text
conductor cycle --dry-run [--repo <name|path>]... [--only <repo>:<issue-id>]... [--config <path>]
```

- No scope: fleet audit. Approval may dispatch only the existing
  `plan.dispatches` bucket; proposals remain proposed.
- `--repo`: only matching repositories enter the persisted plan. Approval
  covers the visible items in those repositories.
- `--only`: only exact repo/issue pairs enter the persisted plan. Approval
  covers those visible items.
- `--only` must also satisfy any `--repo` filter.
- Unknown repo/issue, duplicate selector, excluded repo, or empty result is a
  usage/config error, never an empty successful plan.

The immutable plan records canonical repo paths, issue IDs, scope kind,
selectors, and a hash of the selected item inputs.

## Dispatch semantics

`conductor dispatch <cycle-id>` cannot widen or replace persisted scope.

- Unscoped plan: dispatch only the `dispatches` bucket after approval.
- Explicitly scoped plan: dispatch approved eligible items inside that exact
  scope, including proposals shown in the approval report.
- Items that changed, disappeared, became ineligible, or moved outside budget
  are skipped/deferred and reported; no substitute item is introduced.

The report states whether approval is fleet-audit, repository-scope, or
exact-item-scope and lists the maximum dispatch count. A blanket `approved`
value has meaning only inside that persisted boundary.

## Tests

- Bare approval of an unscoped plan never fires proposals.
- Exact-item scope dispatches only those IDs, even when the scan saw more.
- Repository scope dispatches no other repository.
- Dispatch cannot add selectors or widen a persisted scope.
- Unknown/duplicate/excluded/empty selectors fail closed.
- Item-input hash change prevents launch and reports replan required.
- Existing automatic `dispatches` behavior remains unchanged within budgets.
- The full-fleet 103-proposal shape from `conductor-xa5` is a regression
  fixture: approval launches zero proposals.

## Non-goals

- A new claim mechanism, concurrency model, or triage policy.
- Per-row editing of a persisted plan after approval.
- Treating a report note or free-form model text as a selector.
