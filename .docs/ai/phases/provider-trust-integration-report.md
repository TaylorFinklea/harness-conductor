# Provider trust integration report

Status: complete (2026-07-13)

## Delivered

- strict `bursar/status@2` consumer; v1/missing/malformed/stale/incomplete status fails closed
- one trusted provider snapshot per planning cycle; full candidate evidence persisted in plan/report
- provider-aware read-only `route explain`; tier/ceiling/data-policy gates remain authoritative
- immutable approved fallback identities: model + provider + backend + dispatch ID
- fresh Bursar check immediately before every Claude/Pi/AGY/Codex worker attempt
- sanitized 429/quota/rate-limit observation before approved fallback; provider reset or configured local cooldown
- writeback failure visible in report/ledger; raw worker stderr never enters Bursar request

## Rollout boundary

- schema break: Conductor requires `bursar/status@2` when `budgets.use_bursar = true`; status@1 is deliberately rejected
- install Conductor and Bursar v2 as a paired version; an old Bursar causes terminal provider defers
- `budgets.use_bursar = false` is the only explicit static-caps escape hatch and is labeled in evidence
- no `chezmoi apply`, install, live fleet cycle, or live provider dispatch performed by this slice

## Evidence

- `cargo test`: 262/262 pass
- `cargo clippy --all-targets -- -D warnings`: pass
- `rustfmt --edition 2024 --check src/bursar.rs src/cli.rs src/config.rs src/cycle.rs src/dispatch_cycle.rs src/plan.rs src/route.rs src/triage.rs`: pass
- `cargo fmt -- --check`: known baseline-only diffs remain in `arena.rs`, `fields.rs`, `ledger.rs`, `roster_drift.rs`, and `state.rs`; no provider-trust file reported
- `git diff --check`: pass

## Read-only smoke

```bash
conductor route explain --repo /path/to/repo --tier-floor senior --complexity M --config /Users/tfinklea/git/conductor/conductor.toml
conductor route explain --repo /path/to/repo --tier-floor senior --complexity M --json --config /Users/tfinklea/git/conductor/conductor.toml
```

Automated verification used injected Bursar/dispatch clients and sandbox repos; it required no live provider work.
