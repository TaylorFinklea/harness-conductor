# Decisions

> Architecture decision records. Append-only — one entry per decision.

## [2026-07-01] Rust for the conductor binary

**Context**: Runtime choice delegated by user ("I love rust but decide for me"). Precedents: Go stdlib-only (harness-deck), Rust (larkline).
**Decision**: Rust, mirroring larkline's discipline (unsafe-forbid, LTO release profile, minimal deps: serde/serde_json; no tokio in v1 — dispatch is budget-bounded and serialized per repo, plain threads suffice).
**Alternatives considered**: Go stdlib-only (shares shape with harness-deck, incl. its unmerged Go bd client); TypeScript/Bun atop orchestra.
**Rationale**: User joy on a personal tool they'll maintain; larkline is their proven playbook for exactly this binary shape; the two biggest fleet backlogs (tesela, larkline) are Rust so the implementer fleet demonstrably works in Rust here; the correctness-critical triage core table-drives well under cargo test. Go's only unique edge (reusing harness-deck's bd client) is a small read contract, cheap to reimplement.

## [2026-07-01] Thin composer over bd-native or orchestra-superset

**Context**: Conductor must compose bd, ralph-era backends, orchestra, harness-deck.
**Decision**: Single binary delegating everything to existing tools over subprocess/file contracts; Conductor owns only routing, gates, budgets, serialization, state. Do NOT wrap ralph (Plan-file-scoped, ambiguous exit codes) — invoke backends directly using ralph's proven argv idioms. Do NOT use bd swarm/gate/mol in v1 (unverified semantics).
**Alternatives considered**: bd-native (drive swarm/gate/mol); growing orchestra (TS/Bun) into the conductor.
**Rationale**: Every component already speaks exit-codes/files; the missing piece is exactly the translation layer. bd primitives solve DAG-state, not routing/gates/budgets. orchestra stays a small leaf oracle per its own spec.

## [2026-07-01] Roster is config, scorecard is upstream

**Context**: The live model roster lives in `~/.claude/model-scorecard.md` — session-edited markdown prose.
**Decision**: `conductor.toml` carries the authoritative closed roster (dispatch IDs, tier, ceiling, efficiency); `conductor roster drift` diffs against the scorecard table and warns, never auto-edits.
**Rationale**: Ratchet auto-dispatch is only sound if triage is deterministic and reproducible from config + bead fields; parsing session-owned prose in the dispatch path would let routing silently shift between approval and execution. Also: orchestra's own DEFAULT_MODEL (kimi-k2.7-code) going stale vs the roster is the cautionary tale — Conductor always passes `--model` explicitly.

## [2026-07-01] Routing fields move to bd structured metadata, approval-gated

**Context**: tier_floor/complexity exist today as notes-prose on ~8/231 items; bd has an unused structured-metadata mechanism.
**Decision**: Read metadata first, notes-prose fallback (ranges like `S-M` → upper bound). Conductor may write fields via `bd set-metadata` only after the user approves triage suggestions in a cycle report (user-selected). New canonical keys: `tier_floor`, `complexity`, `verify_cmd`.
**Rationale**: Metadata is queryable/machine-native; prose is fragile. Approval gate keeps fail-closed posture — a mis-triage would otherwise silently steer future auto-dispatch.

## [2026-07-01] hermes-voice and larkline are out of v1

**Context**: User asked whether harness-voice/hermes-voice or larkline belong in Conductor.
**Decision**: Neither is a v1 component. hermes-voice (mid-rebrand to "Harness Voice") is a shipped personal voice UX surface — future (v2+) consumer of conductor events via a thin webhook, never a dispatch backend. larkline is precedent + free display: publishing harness-deck reports with live heartbeats makes conductor state visible in lark-plug-hdeck's "In Flight" view with zero larkline-specific code (its liveness window is 60s — heartbeat faster than that).
**Rationale**: Recon showed neither has any orchestration surface; integration seams are events/reports they already consume.
