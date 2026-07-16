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

## [2026-07-01] Conductor joins the Guildhall suite; two reconciliation additions

**Context**: Conductor is the "master of works" member of the Guildhall suite (charter: `~/git/guildhall`). Two suite-level decisions (rationale in `guildhall/.docs/ai/decisions.md`) add scope to Conductor's backlog.
**Decision**:
1. **Tiered qualitative-review stage** (`conductor-review` bead) — an optional, config-gated pipeline stage after mechanical verify: junior-tier work gets a senior read-only review, senior work optionally a lead review, returning ship|revise + findings. Mirrors what the Lead session did by hand in cycle 1 (caught the `.gitignore` landmine, the agy no-op, evidence quality — none catchable by `verify_cmd`). Enforces `~/AGENTS.md`'s "review only by an equal-or-higher tier" inside the orchestrator. Config `review.enabled` (default true) + `review.min_tier_gap`; one extra dispatch per lower-tier completion, budget-counted.
2. **Bursar budget interface** (`conductor-bursar` bead) — consume `bursar status --json` before metered external dispatch; near-exhausted or "unknown" provider windows down-weight/defer (fail-closed: unknown = spend-cautiously). Retires the static-cap limitation; gives orchestra's dormant `ThrottleState`/`routeBoundary` a real data source via Bursar.
**Alternatives considered**: bake review into the existing m4b verify pipeline (rejected — keep mechanical vs qualitative separable/testable); leave budgets static (rejected — cycle 1 showed real quota exhaustion, agy gemini-flash down ~4.6 days).
**Rationale**: Cycle 1 was Conductor's own design run by hand; both additions crystallize what the manual Lead loop actually did. Cross-member dependency (Bursar must ship first) is noted in bead prose — bd has no cross-repo dep primitive.

## [2026-07-04] Arena mode deliberately routes through Ralph

**Context**: The v1 conductor dispatch path intentionally bypasses Ralph because ordinary fleet dispatch should own backend argv, budgets, and verify/close semantics directly. Arena has a different product question: compare how harnesses use the same model/prompt on the same bead.
**Decision**: Add a separate `conductor arena run` path that creates isolated worktrees, writes byte-identical `.docs/ai/current-state.md`/`loop-prompt.md`, invokes `ralph -n 1 -t <harness>` with model env vars, judges anonymized candidate diffs, and only cherry-picks a strict winner. This does not change the normal cycle/dispatch runner.
**Rationale**: Direct backend dispatch would measure model output while collapsing away the harness variable. Ralph is the existing cross-harness loop contract, so Arena must use it to compare Codex/Pi/OpenCode harness behavior fairly. The apply gate remains Conductor-owned: objective verify, unique safe winner, score threshold, clean worktrees, and real-repo HEAD/clean checks before cherry-pick.

## [2026-07-06] Audit-first roster/router refactor

**Context**: User wants Conductor roster management and routing to be inspectable instead of a black box, while preserving deliberate use of non-Claude models for both cheap offload and outside-perspective/adversarial review. NeuralWatt/Ollama lanes may be valuable fallback capacity even when Bursar has no live telemetry for them.
**Decision**: Keep `conductor.toml` canonical and hand-edited; add read-only validation/explain/dashboard surfaces first. Add explicit provider-outlook policy in config for no-telemetry lanes, explicit bead metadata for `routing_intent` and `provider_risk`, and full per-item candidate audit tables. Phase 1 must not change model selection; later phases may let intent/provider outlook reorder eligible same-tier candidates, with live signals labeled separately from declared policy.
**Alternatives considered**: split roster into a separate config file now; generate config from `~/.claude/model-scorecard.md`; implement behavior-changing provider-aware routing first; infer risk/intent from bead prose.
**Rationale**: The previous closed-roster ADR remains sound: deterministic dispatch must not depend on mutable prose parsing. Audit-first rollout lets humans inspect and tune policy before it changes dispatch behavior. Explicit intent prevents cheapest-model routing from erasing the useful “different model perspective” workflow, and explicit provider outlook avoids inventing telemetry while still making fallback-provider preference reviewable.

## [2026-07-09] GPT-5.6 uses direct Codex dispatch with explicit effort

**Context**: GPT-5.6 Sol, Terra, and Luna expose Codex reasoning levels that Pi cannot faithfully carry. Their chosen effort changes the capability band, especially for Luna.
**Decision**: Add `backend = "codex"` and require `reasoning_effort` on every Codex roster row, Arena profile, and Arena judge. Dispatch invokes `codex exec --model <id> --config model_reasoning_effort=\"<effort>\"`, never inheriting a local global setting. Sol is Lead/XL at `max`; Terra is Lead/XL at `xhigh`; Luna has stable Junior/S `medium` and Senior/L `high` roster rows. Luna accepts through `max` but rejects `ultra`; Sol and Terra accept all closed effort values through `ultra`. Codex counts against the existing metered-external cap and uses Bursar's `codex` provider key.
**Alternatives considered**: Route GPT-5.6 through Pi; use one global Codex effort; represent Luna variants with parenthetical display labels.
**Rationale**: Pi's thinking grammar cannot express the new `max`/`ultra` options, global settings make runs non-reproducible, and parenthetical labels collapse under scorecard normalization. Distinct stable Luna names plus an explicit Reasoning drift column keep routing, Arena, ledger, and scorecard evidence auditable.

## [2026-07-13] Provider state is fail-closed at plan and dispatch

**Context**: Bursar status was checked only at dispatch, missing Bursar fell
back to static caps, and a persisted 429 with no percentage could still be
retried.
**Decision**: Consume only Bursar status@2. Exhausted, unknown, missing,
malformed, stale, and unsupported status defer when Bursar is enabled;
`use_bursar=false` is the sole explicit static-caps override. Persist provider
decisions in plans, recheck before launch, and write classified runtime 429s
back before fallback. Details: `phases/provider-trust-integration-spec.md`.
**Alternatives considered**: Keep late warnings; fail open for unknown; encode
quota guesses in roster policy.
**Rationale**: Dispatch trust depends on provider truth being part of the
approved route. Explicit static mode remains available without letting missing
infrastructure silently change policy.

## [2026-07-13] Adversarial review is an isolated N-plus-one Conductor workflow

**Context**: Cross-provider architecture critiques were valuable but required
repeated prompts and ad-hoc model selection. Putting the logic in a skill would
duplicate Conductor's roster, provider, approval, ledger, and report policy.
**Decision**: Add a separate read-only `adversarial-review` command: N Senior
or Lead reviewers on N distinct providers plus one additional Lead judge. It
shares only closed-roster/provider/report/ledger primitives with Conductor and
does no cycle, bd, git, worktree, or apply operation. The approval pins the
artifact hash, panel, fallbacks, judge, and limits. Details:
`phases/adversarial-design-review-spec.md`.
**Alternatives considered**: Prose-only cross-harness skill; separate review
driver; fold review into normal cycle or Arena.
**Rationale**: A dedicated command is independently testable and inspectable
without creating a second router or increasing the normal cycle's black-box
surface.

## [2026-07-13] Approval scope is persisted and cannot widen at dispatch

**Context**: `conductor-xa5` showed that one fleet-wide approval could launch
every proposal observed under `~/git`.
**Decision**: Unscoped approval may launch only the existing dispatch bucket.
Explicit repo/item selectors are persisted in the plan and approval may cover
proposals only inside that immutable scope. Dispatch cannot add selectors or
substitute items. Each authorized item carries a SHA-256 digest over a
deterministically serialized, ordered input record. Use the in-process
`sha2 = "0.10"` crate. Details: `phases/bounded-dispatch-approval-spec.md`.
**Alternatives considered**: Keep blanket approval; parse free-form approval
notes; add dispatch-time selectors that were not part of the plan; use
process-dependent standard hashing; shell out to `shasum`.
**Rationale**: An approval is meaningful only when its maximum blast radius is
visible and immutable before the user grants it. Standard hashing is not a
stable cross-process contract, and a subprocess would add platform and PATH
failure modes to a correctness boundary.

## [2026-07-13] Roster enablement is config-level, with a provider gate

**Context**: Taking a model or a whole provider lane out of rotation meant
deleting `[[roster]]` rows and losing their config — and `fallback` chains name
roster entries, so a deletion silently orphans other models' chains.
**Decision**: `[[roster]]` rows gain an optional `enabled` (default `true`); a
new first-class `[[provider]]` table gains the same. `effective_enabled =
roster.enabled && provider.enabled` is resolved at parse time. A non-empty
`provider` MUST resolve to a declared `[[provider]]` block (fail closed on
typos); an empty `provider` bypasses the gate (legacy/test shape). A disabled
model is **never selected** and is **skipped** in the fallback walk — the same
rule, so there is no special case for `chain[0]`. Manual `enabled` is the hard
off knob; Bursar's per-cycle `Defer` remains the soft one. Details:
`phases/roster-tui-spec.md`.
**Alternatives considered**: Delete-only (loses config); a runtime overlay file
(leaves `conductor.toml` non-authoritative); "disabled primary is still
selectable, dispatch walks to its first enabled fallback" (routing-alias
framing).
**Rationale**: Rejected the routing-alias framing because `select_candidate`
must return a model that will actually run — otherwise the ledger names a model
that never executed. Provider is the natural toggle unit because it is the unit
that actually goes down (quota, rate limit), and it is where `provider_policy`
(conductor-d5j) will land.

## [2026-07-13] `enabled` must NOT enter `candidate_rejection`

**Context**: `candidate_rejection` (`triage.rs:183`) is shared by
`select_candidate`, the fallback walk, and `next_eligible_roster` — so folding
the enabled check into it is the obvious implementation.
**Decision**: Do not. Keep a separate effective-enabled predicate applied
*after* `candidate_rejection`, and add `Flag::AllDisabled` for "candidates
qualify but all are dark". A disabled link in the walk is a hard skip
(`record_fallback_skip`), never the Bursar `Deferred` path.
**Alternatives considered**: Fold it in (one predicate, less code).
**Rationale**: Folding it in makes `select_candidate` return `None` for a fully
darkened tier, and `route` flags that as `Flag::OverCeiling` (`triage.rs:351`) —
reporting "the operator turned these off" as "this item is too hard." Silent
misattribution, and worse for a ratchet-unlocked auto-dispatch item.

## [2026-07-13] TOML write-back is a line-span editor gated by structural equivalence

**Context**: A roster TUI must write `conductor.toml` — 535 lines carrying
load-bearing comments. `config.rs` hand-rolls a read-only TOML parser and the
crate holds only three dependencies.
**Decision**: Hand-roll a line-span editor (`src/config_edit.rs`) that splices
only the lines it touches, so comments survive by construction. Take ratatui +
crossterm for the UI (feature-gated). Do **not** add `toml_edit`. Every write is
gated by a **structural-equivalence check**: re-parse and assert the resulting
`Config` differs from the pre-edit `Config` only in the intended field.
**Alternatives considered**: `toml_edit` (battle-tested round-tripping);
hand-roll the terminal layer too; gate writes on `parse_str` alone.
**Rationale**: `toml_edit` would put two TOML semantics in one tree that can
disagree — the TUI could write what `config.rs` rejects. Rendering, by contrast,
is toil with no domain value, so ratatui is worth the dep. Critically, gating on
`parse_str` alone is **insufficient**: the parser accepts `[[roster]] # comment`
(`config.rs:1944`), so an indexer whose header match is stricter than the
parser's mis-attributes keys to the previous block and silently edits the WRONG
model while still emitting valid TOML. Parseability is not correctness — hence
structural equivalence. (Found by adversarial review, opencode-go/glm-5.2.)

## [2026-07-14] backnotprop/orchestrator is mined for design, never adopted as a dependency

**Context**: `backnotprop/orchestrator` (BUSL-1.1, ~31k LoC TS) is a kubectl-style
process-supervision layer for agent workers across Claude Code, Codex, Copilot,
Grok, and Pi. It explicitly disclaims Conductor's layer — ADR 0008 "do not require
structured worker output in v1", ADR 0011 "parent-directed, no prebaked recipes" —
so it competes with `dispatch.rs` alone, not with scan/triage/verify/ledger. Its
README and ADR titles advertise worktree isolation, session resume, and provider-limit
intelligence: three things we want. A title-deep comparison recommended adopting it
behind the `Exec` trait (`dispatch.rs:129`).

**Decision**: **Mine the design; take no dependency.** Reading the source
(`conductor-iz7`, pinned `583acf4`, memo `docs/notes/orchestrator-recon.md`) refuted
the premise. Concretely: (a) take their provider-limit detectors into Bursar —
endpoints, the two-tier CLI fallback, the window+reset shape, the typed auth-failure
taxonomy (`bursar-ejf`); (b) build a **native Rust client** over Codex's *own*
app-server protocol (`conductor-2d4`); (c) borrow the tri-state degrade-honestly
catalog shape for `roster_drift`; (d) build worktree isolation clean (`conductor-fia`).
Close `conductor-kfq` (wrap-orchestrator spike) as wont-fix. Do not vendor or copy
their code — BUSL-1.1 would carry into our tree.

**Alternatives considered**: Adopt as one `Exec` impl behind the existing trait
(the original recommendation); reject wholesale and revisit later.

**Rationale**: Adoption's two justifying capabilities did not survive verification.
**Worktree isolation does not exist** — `supportsWorktree`/`CwdPolicy` are unconsumed
type surface (`runtime/types.ts:30-31,40`) and orchestrator never invokes git at all;
our Arena worktrees already exceed it. **Session resume is codex-app-server-only** —
Claude Code, Pi, Copilot and Grok all use the process executor, whose handle is
`{completed, interrupt}` (`process.ts:399-401`), one-shot and identical to
`dispatch.rs`; `sendMessage?`/`startGoal?` are optional members precisely because
most executors lack them (`executors/types.ts:44-48`). Wrapping would therefore buy
nothing for three of our four backends. And the resume protocol we *do* want —
`thread/start`, `thread/resume`, `turn/start`, `account/rateLimits/read` — is
**Codex's API, not orchestrator's**; we already depend on the `codex` binary, so we
can speak it from Rust with no BUSL exposure and no Node toolchain.

The disqualifier is verification. Their success oracle is `code === 0`
(`process.ts:257-264`), cross-checked only against the worker's own output stream —
so an **agy quota-exhausted no-op reports `succeeded`**. That is the exact failure the
charter names ("exit codes are testimony; artifacts are evidence"), and `dispatch.rs`'s
`CommitProbe` classify (`dispatch.rs:140,483`, and the `exit_zero_with_no_new_commit_*`
tests) is materially better: it at least demands an artifact. We would have had to
override their status model on day one. A supervisor whose notion of success we must
overrule is not a supervisor we should depend on — it is a design to read and a
protocol to reimplement.

**Correction (same day, on discovering `conductor-1i9`)**: an earlier draft of this ADR
called our classify "strictly better." That overclaims. `conductor-1i9` establishes that
`CommitProbe` is **itself forgeable** — it declares success on *any* HEAD change
(`dispatch.rs:350-352`, `verify.rs:161-162,223`), not on the worker's *own* commit, so a
concurrent commit satisfies it. Ours demands an artifact but does not check who produced
it; theirs demands nothing but an exit code. The decision above is unchanged — we would
still be importing the weaker of two flawed oracles, and would still have to override it
— but the honest statement is that **both success models are broken, ours less badly**,
and `conductor-1i9` is the P0 that fixes ours. This ADR is not a claim that our
verification is sound; it is a claim that theirs is not worth adopting.

## [2026-07-15] Codex is the terminal approved fallback for fleet ownership

**Context**: Live Bursar evidence can fail closed for otherwise usable provider
accounts: Anthropic currently returns an authentication error, opencode-go has
no positive availability signal, and Ollama Cloud is not represented in
`bursar/status@2`. The GPT-5.6 Codex lane is positively healthy and already in
the closed roster, but was outside the legacy primary models' fallback chains.

**Decision**: With explicit human approval, append the matching-tier GPT-5.6
Codex profile to every non-Codex roster chain. Preserve opencode-go and Ollama
Cloud as preferred cheap-work lanes and NeuralWatt as reserve; Codex remains the
terminal fallback. Lead chains may use both Terra and Sol in capability order.

**Alternatives considered**: Disable Bursar; record operator assertions as
provider health; promote Codex to the default primary; bypass Conductor and
dispatch the blocked Bead manually.

**Rationale**: A configured fallback is reviewable policy and becomes part of
the immutable approved route. It restores forward progress from real positive
Codex evidence without treating prose as quota truth, weakening fail-closed
provider checks, or moving claim/verify/close ownership out of Conductor.
