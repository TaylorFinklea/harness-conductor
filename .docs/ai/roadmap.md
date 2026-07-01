# Roadmap

> Durable goals and milestones. Updated when scope changes, not every session.

## Vision

Conductor: a single Rust binary that runs autonomous work-routing cycles over the ~24 beads-tracked repos under `~/git` — scan → triage → plan → approval → dispatch → verify → report — composing bd, pi/agy/claude, orchestra, and harness-deck over subprocess/file contracts. Spec: `phases/conductor-v1-spec.md`.

## Now / Next / Later

### Now
- [ ] Work the beads backlog via `bd ready` (18 items seeded 2026-07-01, IDs `conductor-m0a`…`conductor-m6` + `conductor-prompt`/`conductor-rev1`/`conductor-bdro`). Unblocked roots: `conductor-m0a` (senior), `conductor-prompt` (lead), `conductor-bdro` (junior). Routing fields are in bd metadata; every bead's Verify is its `verify_cmd`.

### Next
- [ ] M3 dry-run cycle has a human-verify tail (report renders on dashboard) — see `conductor-m3b` notes

### Later
- [ ] M3 dry-run cycle → M4 dispatch+verify → M5 triage backfill → M6 ratchet
- [ ] Post-v1 spikes: bd swarm/gate/mol evaluation; hermes-voice notification channel; SSE response push

## Milestones

See `phases/conductor-v1-spec.md` § Milestones (M0–M6) — each has scope + Verify there; beads are the per-item backlog.

## Backlog

> Lives in beads (`bd ready`) once the repo is initialized — not in this file.

## Constraints

- Invariants in spec § Invariants are non-negotiable (closed roster, tier_floor gate, fail-closed, no push, no chezmoi, one writer per repo).
- Implementation is fleet-driven: Sonnet-5/GPT-5.5/minimax et al. own Senior/Junior beads; Opus/Fable own Lead beads. Mis-triaging down is the expensive error.
