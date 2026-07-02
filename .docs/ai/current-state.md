# Current State

> Updated at the end of every work session. Read this first.

## Active Branch

`main`

## Last Session Summary

**Date**: 2026-07-01

- Recon fan-out over all fleet components (bd, ralph, orchestra, harness-deck, tiers/scorecard, hermes-voice, larkline, fleet inventory) — ground truth baked into spec § Ground truth
- User decisions locked: ratchet autonomy, manual CLI trigger, Rust, moderate budgets, metadata-writes-after-approval
- Spec written + user-approved: `phases/conductor-v1-spec.md`
- `bd init --stealth -p conductor`; 18 beads seeded from spec with metadata (tier_floor/complexity/verify_cmd) + 24 dep edges
- Fleet dispatch cycle 1: 6/8 CLOSED — m0a (minimax), prompt (sonnet-5), bdro (minimax, escalated from agy), m0b (glm), m1a (gpt-5.5), m2a (minimax 2c50adf). Remaining: m1b→qwen, m2b→sonnet-5
- Conductor is now the "master of works" member of the **Guildhall** suite (charter: `~/git/guildhall`); 2 reconciliation beads added: conductor-review (tiered review stage) + conductor-bursar (affordability budgeting), both blocked on M4
- New bead conductor-agy: root-caused today's agy no-ops (quota exhaustion, fail-open exit 0); gemini-flash out until ~2026-07-06
- Incident (resolved): recon subagent ran `bd ready --claim` in tesela → reverted (stray `started_at` cosmetic)

## Build Status

- cargo build/clippy -D warnings/test: clean at 2c50adf (46 tests). Known: bd real-subprocess test flakes on embedded Dolt init, passes on retry (noted on conductor-m1a)

## Blockers

- None
