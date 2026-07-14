# current-state.md — conductor

Branch: codex/provider-trust-p1
`cargo test` 262/262; strict Clippy clean.
Rebrand `harness-conductor` → `conductor` COMPLETE (2026-07-12): source refs, GitHub repos (incl. `backlog-conductor`), dir move, `chezmoi apply`, live HOME verified clean; `conductor config check` passes.

## Plan

- [x] conductor-61f: consume Bursar status@2. Verify: `cargo test`
- [x] conductor-sb6: provider-aware route explain. Verify: `cargo test triage::tests route::tests cli::tests::route_`
- [x] conductor-oxm: persist evidence, dispatch recheck, 429 writeback. Verify: `cargo test cycle::tests dispatch_cycle::tests bursar::tests`
- [x] conductor-mus: provider-trust final gates and handoff. Verify: `cargo test`
- [ ] conductor-6dv: immutable approval scope and item hashes. Verify: `cargo test plan::tests`
- [ ] conductor-5ij: cycle selector parsing and enforcement. Verify: `cargo test cycle::tests deck::tests cli::tests`
- [ ] conductor-xa5: bind dispatch to persisted scope; proposals inert when unscoped. Verify: `cargo test`
- [ ] conductor-8z8: bounded-approval final gates and report. Verify: `cargo test`
- [ ] conductor-i8r: adversarial config and artifact snapshot. Verify: `cargo test config::tests adversarial::tests::artifact_`
- [ ] conductor-0zv: distinct-provider reviewer panel plus Lead judge. Verify: `cargo test adversarial::tests::panel_ bursar::tests`
- [ ] conductor-b35: immutable review approval envelope. Verify: `cargo test adversarial::tests::approval_ deck::tests`
- [ ] conductor-2cr: bounded read-only reviewers and schema repair. Verify: `cargo test adversarial::tests::reviewer_ adversarial::tests::parallel_ dispatch::tests`
- [ ] conductor-vly: anonymous N+1 judge, ledger, and reports. Verify: `cargo test adversarial::tests ledger::tests deck::tests`
- [ ] conductor-j84: adversarial CLI, mutation proof, final gates, and handoff. Verify: `cargo test`

## Blockers

## Open questions
