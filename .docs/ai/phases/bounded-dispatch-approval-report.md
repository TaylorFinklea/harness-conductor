# Bounded dispatch approval report

Status: complete (2026-07-13)

## Delivered

- immutable `fleet-audit`, `repository-scope`, and `exact-item-scope` plans with canonical selectors and maximum dispatch counts
- deterministic SHA-256 authorization over repository, issue content, routing, Verify command, selected model, and ordered fallback envelope
- `cycle --dry-run --repo/--only` selection that fails closed on unknown, duplicate, excluded, incompatible, or empty scope
- unscoped approval launches only the persisted automatic-dispatch bucket; proposals remain inert
- explicit approval launches only visible items inside the persisted repository or exact-item scope
- pre-claim `show`/`ready`/routing/hash checks and post-claim rehash; changed state reports `REPLAN_REQUIRED`, introduces no substitute, and launches no worker
- `conductor dispatch` rejects selectors, so dispatch cannot widen an approved plan

## Maximum blast-radius guarantee

An approval can launch no more than the plan's persisted
`approval_scope.max_dispatch_count`. Fleet-audit approval never turns proposals
into dispatches. Explicit approval is constrained by canonical repository or
repository-plus-issue selectors, one authorization hash per launchable item,
and the approved provider fallback envelope. Normal claim, worker, mechanical
verify, qualitative review, and close behavior starts only after those checks.

## Regression evidence

- the `conductor-xa5` fleet shape persists 103 proposals with a maximum dispatch count of zero; approval launches none
- repository and exact-item scopes reject another repository or bead
- changed authorization content prevents claim and spawn
- a change during claim causes release and no spawn
- dispatch-time `--repo` and `--only` arguments exit 2

## Verification

- `cargo test`: 275 unit tests plus 1 integration test pass
- `cargo clippy --all-targets --all-features -- -D warnings`: pass
- `rustfmt --edition 2024 --check src/dispatch_cycle.rs src/cli.rs`: pass
- `cargo fmt -- --check`: only known unrelated baseline diffs in `arena.rs`, `fields.rs`, `ledger.rs`, `roster_drift.rs`, and `state.rs`
- `git diff --check`: pass

No live fleet cycle, provider dispatch, `chezmoi apply`, or push was performed.
