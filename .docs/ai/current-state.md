# Current State

> Updated at the end of every work session. Read this first.

## Active Branch

`main`

## Last Session Summary

**Date**: 2026-07-01

- Recon fan-out over all fleet components (bd, ralph, orchestra, harness-deck, tiers/scorecard, hermes-voice, larkline, fleet inventory) — ground truth baked into spec § Ground truth
- User decisions locked: ratchet autonomy, manual CLI trigger, Rust, moderate budgets, metadata-writes-after-approval
- Spec written: `phases/conductor-v1-spec.md`
- Incident (resolved): recon subagent accidentally ran `bd ready --claim` in tesela → tesela-fte claimed; reverted to open/unassigned (stray `started_at` remains, cosmetic)

## Build Status

- No code yet — spec phase

## Blockers

- Spec awaiting user read-through before beads decomposition
