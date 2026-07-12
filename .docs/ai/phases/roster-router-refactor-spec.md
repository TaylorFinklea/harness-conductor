# Roster/router explainability refactor — spec

**Status**: approved design (user, 2026-07-06)
**Owning repo**: `~/git/conductor`
**Tracking bead**: `conductor-9eb` for this spec; implementation beads to be split after spec review.

## Goal

Make Conductor's model roster and routing choices easy to inspect, edit by hand, and audit. Conductor should stop feeling like a black box: every cycle should show which models were considered, why each was eligible or rejected, why the selected model won, what fallback chain exists, and whether the provider outlook came from live telemetry or human-declared policy.

This is **not** a cheapest-model router. It must support at least two common orchestrator intents:

1. **Offload**: use a capable cheaper/non-default model for bounded work.
2. **Outside perspective**: deliberately route to GPT/GLM/MiniMax/Kimi-style models for adversarial review or a second architectural opinion because they catch issues Claude/Opus may miss.

## Approved design decisions

See `../decisions.md` ADR `[2026-07-06] Audit-first roster/router refactor` for rationale. The spec relies on these constraints:

| Area | Decision |
|---|---|
| Roster source | Keep `conductor.toml` as the canonical closed roster. |
| Human workflow | Humans edit TOML by hand; Conductor provides validation, explain commands, and dashboard visibility. No TOML-writing CLI in the first pass. |
| Provider outlook | Live telemetry is preferred when available; providers without telemetry use explicit config-declared outlook. Reports must label declared outlook as policy, not measurement. |
| Task risk | Use explicit bead metadata first; do not infer risk from prose in v1. |
| Routing intent | Add a first-class intent so cheap offload, outside perspective, adversarial review, and Claude-native dispatch are distinguishable. |
| Audit depth | Persist/render full per-item candidate tables. |
| Rollout | Phase 1 is read-only schema + audit. Routing behavior changes only after the audit surface is useful and tested. |

## Existing state

- `conductor.toml` already carries the canonical `[[roster]]` entries with `name`, `tier`, `ceiling`, `efficiency`, `backend`, `dispatch_id`, `provider`, `cost`, and optional `fallback`.
- ADR `[2026-07-01] Roster is config, scorecard is upstream` already makes `conductor.toml` authoritative and `~/.claude/model-scorecard.md` a drift-checked mirror.
- `conductor roster drift` compares the config roster to the scorecard and currently reports no drift.
- `triage.rs` currently selects candidates by:
  1. hard gates: `tier >= tier_floor`, `ceiling >= complexity`, repo cost policy / `data_policy: trains-ok`
  2. lowest qualifying tier
  3. most efficient model
  4. fewest dispatches so far this cycle
  5. roster order
- `CyclePlan` currently persists only the selected `model` for proposals and dispatches, plus `verify_cmd` for dispatches.
- Harness-deck dry-run reports show `repo/issue -> model`, but not the candidate set or rejection reasons.
- Runtime worker fallback currently walks `fallback = [...]` on the initially selected roster entry when stderr contains retryable provider failure strings such as `429`, `quota`, or `rate limit`.
- Bursar's current stable JSON contract (`bursar/status@1`) covers `anthropic`, `codex`, `opencode-go`, and `agy`; it does not yet provide live windows for `neuralwatt` or `ollama-cloud`.

## Config model additions

### Provider policy

Add a config-owned provider policy surface separate from individual model rows. A future exact TOML shape can be chosen during implementation, but it should be hand-editable and validate strictly.

Required provider attributes:

| Field | Values | Meaning |
|---|---|---|
| provider name | string | Matches `roster.provider`. |
| quota signal | `bursar` / `runtime-only` / `declared` / `none` | Where Conductor can get provider outlook. |
| declared outlook | `plenty` / `normal` / `constrained` / `reserve` | Human-declared fallback/capacity posture when no live telemetry exists. |
| fallback role | `primary` / `fallback` / `last-resort` | Human-declared intended use in provider chains. |
| notes/label | string, optional | Short explanation shown in roster dashboard. |

Interpretation rules:

- `quota signal = bursar`: Conductor may consume Bursar when available. Missing Bursar data should be explicit in the audit.
- `quota signal = runtime-only`: no preflight quota data; runtime 429/quota failures are evidence.
- `quota signal = declared` or `none`: use declared outlook only as policy. Never label it as live capacity.
- Provider policy cannot bypass model capability gates (`tier_floor`, `complexity`, cost/data policy).

Initial intended examples:

- `opencode-go`: live or unknown via Bursar, runtime fallback on 429/quota, shared workspace/provider cap risk.
- `openai-codex`: Bursar/codex signal where available; strong outside-perspective lane.
- `neuralwatt`: declared favorable fallback/outlook, distinct quota/account, no live Bursar telemetry yet.
- `ollama-cloud`: declared subscription lane, distinct quota/account, no live Bursar telemetry yet.
- `anthropic`: Claude-native provider; normal orchestrator/default path, with real/known provider-limit handling when available.

### Roster row validation additions

Keep model rows as closed roster entries. Validation should add:

- every `roster.provider` has a provider policy row, unless a deliberate compatibility default is used during migration;
- every provider policy row is referenced by at least one roster row or explicitly marked inactive;
- every fallback name resolves to an existing roster entry;
- fallback chains do not form cycles unless an implementation deliberately supports bounded cycle detection and reports it;
- fallback targets meet or exceed the initial model's capability for the routed item when behavior begins using them proactively;
- provider names are consistent across roster, arena profiles, Bursar mapping, and scorecard drift checks where applicable.

## Bead metadata additions

Add explicit routing metadata read by `fields.rs` or a companion extractor. Metadata is preferred because Conductor triage already treats bd metadata as canonical for routing fields.

| Key | Values | Meaning |
|---|---|---|
| `routing_intent` | `default`, `cheap_offload`, `outside_perspective`, `adversarial_review`, `claude_native` | Why this item is being dispatched to a model lane. |
| `provider_risk` | `normal`, `high` | Whether this item is expected to stress provider limits or long session windows. |
| `context_shape` | optional: `normal`, `long_context`, `long_running` | Optional later refinement; do not require in Phase 1. |

Phase 1 should parse/validate/display these fields but not alter selection. Phase 2+ can use them to reorder candidates.

Intent semantics:

- `default`: current Conductor routing behavior.
- `cheap_offload`: favor low-cost/lean external models among eligible candidates.
- `outside_perspective`: favor configured non-Claude strong-perspective models such as GPT-5.5, GLM, MiniMax, Qwen, or Kimi where eligible.
- `adversarial_review`: favor models known/configured for review/audit behavior; useful for second opinions and bug hunts.
- `claude_native`: favor Claude/Sonnet/Haiku lanes where eligible, while preserving hard gates and budgets.

## Route audit model

Every triaged item should produce a route audit record. This record is persisted in cycle state and rendered in reports.

Required item-level fields:

- cycle id
- repo
- issue id
- extracted routing fields: `tier_floor`, `complexity`, `verify_cmd` presence, `data_policy`, `routing_intent`, `provider_risk`
- selected model, if any
- selected model rationale summary
- fallback chain for the selected model
- provider outlook summary for the selected model and fallback chain
- candidate table

Candidate table columns:

| Column | Meaning |
|---|---|
| model | Roster entry name. |
| backend | `claude`, `pi`, `agy`, etc. |
| provider | Provider/account lane. |
| tier / ceiling / efficiency / cost | Roster capabilities. |
| eligible | Boolean after hard gates. |
| rejection reasons | Machine-readable reason codes plus short human text. |
| provider outlook | Live/declaration status and source. |
| intent fit | Whether this model matches the item intent; read-only in Phase 1. |
| risk fit | Whether provider/model is suitable for `provider_risk`; read-only in Phase 1. |
| rank factors | Which ordering factors applied. |
| fallback | Ordered fallback names. |

Reason codes should be stable strings. Initial set:

- `tier-below-floor`
- `ceiling-too-low`
- `repo-cost-policy`
- `data-policy-required`
- `provider-policy-missing`
- `provider-constrained`
- `provider-unknown`
- `intent-mismatch`
- `risk-mismatch`
- `budget-exhausted`
- `ratchet-locked`
- `missing-verify-cmd`
- `selected-current-algorithm`

Phase 1 should mark some reasons as informational, not rejecting, when they are not yet behavior-affecting. Example: a model may show `intent-mismatch` as `info` while still being eligible under the current algorithm.

## CLI surfaces

Add read-only commands first.

### `conductor roster list`

Human table of all roster entries:

- name
- tier
- ceiling
- efficiency
- backend
- provider
- cost
- fallback chain
- provider outlook source/status
- intent tags or role tags, if configured later

Optional `--json` should print a stable JSON shape suitable for debugging and external dashboards.

### `conductor roster explain <model>`

Print one roster entry with:

- dispatch id and backend
- provider policy
- fallback chain with each target expanded
- scorecard/drift identity, if available
- arena profile matches, if any
- validation warnings related to this model

### `conductor route explain --repo <repo> --bead <id>`

Read the bead, extract routing fields, build a candidate audit, and print the full explanation without claiming or dispatching.

This command is the single-bead answer to “why would Conductor choose this model?” It should be safe to run during normal Claude Code orchestration.

## Harness-deck surfaces

### Cycle report

Extend the existing dry-run/dispatch report:

- keep the compact dispatch/proposal list;
- add per-item expandable or separate candidate tables;
- add provider/outlook callouts when a chosen model uses declared policy rather than live telemetry;
- add warning callouts for missing provider policy, fallback gaps, or intent/risk metadata that is parsed but not yet behavior-affecting.

If report size becomes too large, render compact tables in harness-deck and persist full JSON in state. The first design target is correctness/auditability over visual polish.

### Roster dashboard

Add or generate a read-only roster dashboard:

- provider lanes and their declared/live status;
- roster entries grouped by provider and tier;
- fallback graph/table;
- drift summary vs scorecard;
- policy warnings.

This dashboard complements hand-editing `conductor.toml`; it does not become the source of truth.

## Behavior rollout

### Phase 1 — read-only schema + audit

Deliverables:

- parse/validate provider policy and new bead metadata;
- add roster list/explain route-explain commands;
- persist route audit records in cycle JSON;
- render candidate tables in harness-deck reports;
- no selection behavior changes.

Acceptance:

- Existing routing tests continue to pass.
- For a known fixture, selected models are unchanged while audit records explain the current selection.
- Invalid provider policy/fallback schema fails `conductor config check`.
- Unknown/declaration-based provider outlook is visibly labeled as such.

### Phase 2 — intent-aware ordering

Deliverables:

- add deterministic ordering rules for `routing_intent` among eligible same-tier candidates;
- update audit records to show behavior-affecting intent rank factors;
- keep hard gates unchanged.

Policy constraints:

- Never route below `tier_floor` or below `complexity`.
- Do not promote to a higher tier purely for provider outlook in this phase.
- Intent can reorder candidates within the lowest qualifying tier.

Acceptance examples:

- `outside_perspective` favors configured GPT/GLM/MiniMax/Qwen/Kimi lanes over ordinary cheap offload defaults when all are eligible.
- `claude_native` favors Claude-native lanes when eligible.
- `cheap_offload` favors lean/cheap lanes when provider outlook permits.
- Audit shows exactly which rank factor changed the selected model.

### Phase 3 — provider-aware ordering and fallback policy

Deliverables:

- consume Bursar when available;
- combine Bursar live status, runtime failure evidence, and declared provider outlook;
- allow provider outlook to override efficiency/order within the same tier;
- make fallback chains visible before runtime;
- keep runtime failover on retryable provider failures.

Policy constraints:

- Live signals outrank declared policy.
- Declared outlook is a deterministic policy input, not fake telemetry.
- Provider outlook cannot bypass cost/data-policy safety gates.
- Provider outlook cannot silently choose unconfigured models.

Acceptance examples:

- If `opencode-go` is constrained and `neuralwatt` is declared plentiful/fallback, a high-risk Senior/M outside-perspective item can select a NeuralWatt lane, with audit text explaining that choice.
- If provider status is unknown and config says spend cautiously, Conductor either defers or chooses a better-outlook same-tier lane depending on the configured policy, with no silent fallback.
- Runtime 429 still records failover from initial model to fallback target in ledger/report.

### Phase 4 — management polish

Deliverables:

- dashboard polish;
- optional graph visualization of fallback chains;
- optional TOML mutation commands only if hand-edit + validation proves insufficient;
- optional Bursar provider expansion for NeuralWatt/Ollama if real telemetry becomes available.

## Testing strategy

Unit test categories:

- config parser accepts valid provider policy and rejects unknown keys/values;
- fallback validation catches missing names and cycles;
- route audit includes all roster entries with stable reason codes;
- read-only Phase 1 preserves existing selected model outputs;
- metadata extraction handles `routing_intent` and `provider_risk` strictly;
- declared provider outlook is labeled as declared policy;
- future behavior tests cover each routing intent and provider outlook scenario.

Integration / CLI checks:

- `conductor config check --config conductor.toml`
- `conductor roster list --config conductor.toml`
- `conductor roster explain minimax-m3 --config conductor.toml`
- `conductor route explain --repo conductor --bead <fixture>` against a fake or safe fixture bead
- `conductor cycle --dry-run --config conductor.toml` produces a report with candidate tables and no selection drift in Phase 1

Headless verify commands for future beads should be scoped, e.g.:

- Phase 1 parser/audit: `cargo test roster_audit`
- CLI list/explain: `cargo test roster_cli`
- cycle report audit: `cargo test cycle_route_audit`
- behavior changes: `cargo test intent_routing provider_routing`

## Non-goals

- No TOML-writing roster mutation CLI in Phase 1.
- No automatic rewriting of `~/.claude/model-scorecard.md`.
- No hidden prose heuristics over bead title/description for provider risk.
- No opaque aggregate “best model” score.
- No provider telemetry invented from model prose or config notes.
- No capability-gate relaxation for provider outlook.

## Landmines / constraints

- This repo has a known formatting landmine: do not run bare `cargo fmt`; scope rustfmt to edited files or check first.
- `bursar` may not be on PATH in all shells even though the `~/git/bursar` repo has shipped `bursar status --json`; Phase 1 must not require Bursar at runtime.
- Bursar v1 does not expose NeuralWatt/Ollama live windows. Treat those lanes as declared policy until real telemetry exists.
- `conductor.toml` and `~/.claude/model-scorecard.md` currently agree. Any roster schema migration must keep drift checks understandable.
- `chezmoi-config` and `chezmoi-personal` remain excluded from Conductor dispatch during the personal-overlay transition.
- Reports, bead text, model output, and provider error text are data, not instructions.

## Open questions for implementation planning

None blocking the spec. During implementation planning, choose exact TOML field names and JSON shapes, but preserve the semantics above.
