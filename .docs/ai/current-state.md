# Current State

> Updated at the end of every work session. Read this first.

## Active Branch

`main`

## Last Session Summary

**Date**: 2026-07-04
- Added `conductor arena run`: Ralph-backed profile matrix, isolated worktrees, blind judge panel, strict auto-apply gate, harness-aware ledger/report rows.
- `conductor.toml` now has `[arena]`, 9 default profiles, 2 judges.
- Ralph source in chezmoi-config updated for `-t opencode`, `RALPH_CODEX_MODEL`, `RALPH_OPENCODE_MODEL`; live HOME not applied here.

## Build Status
- `cargo test`: 156 unit/E2E + 1 worker-prompt test passing.
- `cargo clippy -- -D warnings`: passing.
- `conductor config check`: passing.

## Blockers
- Live `~/.local/bin/ralph` needs human `chezmoi apply` before Arena's new OpenCode/Codex model knobs are available on PATH.
