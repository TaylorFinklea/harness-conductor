# current-state.md — conductor

Branch: main
`cargo test` baseline 237/238 — 1 pre-existing env failure (`harness_deck_validate`; `harness-deck` not on PATH).

## Plan

Active: `conductor-pp0` — roster TUI phase 1. Exact-code plan: `phases/roster-tui-phase1-plan.md`.

- [ ] T1 `[[provider]]` struct + parsing + seed 7 lanes in conductor.toml. Verify: `cargo test --lib config::tests`
- [ ] T2 `enabled` on roster rows (default true) + 3 test-helper literals. Verify: `cargo test`
- [ ] T3 provider resolution + `effective_enabled`; undeclared provider fails closed. Verify: `cargo run -- config check --config conductor.toml`
- [ ] T4 `Selection` enum + `Flag::AllDisabled`; disabled model never selected. Verify: `cargo test --lib triage::tests`
- [ ] T5 fallback walk skips disabled links. Verify: `cargo test`
- [ ] T6 `config check` reports enabled counts per tier. Verify: `cargo run -- config check --config conductor.toml`

Order is load-bearing: seed (T1) before validation (T3), else the live config stops parsing.
Phases 2/3 (`conductor-68z`, `conductor-c4x`) are bd-blocked behind pp0; each needs its own plan.

## Blockers

## Open questions

- T4 reserves an operator call: what should a cycle DO on `Flag::AllDisabled` — skip+report (current default), escalate a tier (spends money the operator avoided by darkening the lane), or hard-fail? Raise it, don't decide it.
- `b3631a0` (`--help` subcommand) landed mid-session and is not mine — assumed user/parallel session.
