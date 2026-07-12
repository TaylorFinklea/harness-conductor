# Roadmap

> Durable goals and milestones. Updated when scope changes, not every session.

## Vision

Conductor: a single Rust binary that runs autonomous work-routing cycles over the ~24 beads-tracked repos under `~/git` — scan → triage → plan → approval → dispatch → verify → report — composing bd, pi/agy/claude, orchestra, and harness-deck over subprocess/file contracts. Spec: `phases/conductor-v1-spec.md`.

## Now / Next / Later

### Now
- [ ] **Rebrand cutover `harness-conductor` → `conductor` — human steps remain** (2026-07-12). Done: in-repo refs + chezmoi-personal source refs (commit `f95115b`) retargeted to `~/git/conductor`; GitHub `TaylorFinklea/harness-conductor` renamed → `conductor`, local `origin` updated. **Remaining (run in this order, none can be done from a session whose cwd is the old dir):**
  1. `gh repo rename backlog-conductor --repo TaylorFinklea/backlog-harness-conductor --yes` — **required, not cosmetic:** `restore-beads-backlogs.sh` derives `backlog-<repo>` from the repo dir name, so after the move it resolves `backlog-conductor`; its test already asserts that. Skip this and beads restore 404s.
  2. `mv ~/git/harness-conductor ~/git/conductor`
  3. `chezmoi apply` — publishes the retargeted `ralph`, `gen-scorecard-digest.mjs`, AGENTS.md, and 8 SKILL.md copies to live HOME. **Landmine:** until applied, live `~/.claude/skills/*` and `~/.local/bin/ralph` still point at `/Users/tfinklea/git/harness-conductor/conductor.toml`, which stops existing the moment step 2 runs — so step 3 must not lag step 2.
  - Caveat: Claude Code's memory/session dir is path-keyed (`~/.claude/projects/-Users-tfinklea-git-harness-conductor/`); the move orphans it. Copy it to the `-conductor` name if that history is worth keeping.
  - Preserved deliberately (do not "fix"): `docs/notes/agy-dispatch.md` verbatim CLI transcript, the executed `arena-harness-scorecard-{plan,spec}.md`, and chezmoi-personal `roadmap.md:27` dated prose.
- [ ] `cargo test` has 1 pre-existing env failure: `deck::tests::generated_sample_report_passes_harness_deck_validate` shells `Command::new("harness-deck")`, which is not on PATH (237/238 pass). Fails on a clean tree at `0c801d3` too — unrelated to the rebrand. Either install `harness-deck` or gate the test on the binary being present.
- [ ] Cycle 1 COMPLETE (9 beads closed: m0a, m0b, m1a, m1b, m2a, m2b, prompt, bdro, rev1); `cargo test` passes 84 tests. Live ready queue (`bd ready`, 6 items): `conductor-m4a`/`conductor-m3a` (P1), `conductor-agy`/`conductor-m1c`/`conductor-m0c` (P2), `conductor-cov1` (P3). Routing fields are in bd metadata; every bead's Verify is its `verify_cmd`.

### Next
- [ ] M3 dry-run cycle has a human-verify tail (report renders on dashboard) — see `conductor-m3b` notes. `conductor-guildhall-dogfood` (lead, v1 integration proof) is now bd-blocked on `conductor-m3b` and carries its own human-verify tail (dry-run over 3+ real repos + dashboard spot-check; verify_cmd alone under-covers).

### Later
- [ ] M3 dry-run cycle → M4 dispatch+verify (m4a→m4b→m4c) → `conductor-review` → M5 triage backfill → M6 ratchet. `conductor-review` bumped P2→P1 and now GATES v1-done (user decision 2026-07-02, ADR in guildhall decisions.md); still bd-blocked on m4c + m4b.
- [ ] `conductor-warden` set to deferred (self-labeled v1.5; not in the v1-done clause) — un-defer after conductor-m4c + warden m3/m4/m6.
- [ ] Post-v1 spikes: bd swarm/gate/mol evaluation; hermes-voice notification channel; SSE response push

## Milestones

See `phases/conductor-v1-spec.md` § Milestones (M0–M6) — each has scope + Verify there; beads are the per-item backlog.

## Backlog

> Lives in beads (`bd ready`) once the repo is initialized — not in this file.

## Constraints

- Invariants in spec § Invariants are non-negotiable (closed roster, tier_floor gate, fail-closed, no push, no chezmoi, one writer per repo).
- Implementation is fleet-driven: Sonnet-5/GPT-5.5/minimax et al. own Senior/Junior beads; Opus/Fable own Lead beads. Mis-triaging down is the expensive error.
