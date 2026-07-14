# roster-tui — spec

Status: designed 2026-07-13. Not started.

A terminal UI for managing the Conductor model roster: toggle models and
providers in/out, edit fallback chains, add models, all against a live view of
provider health and scorecard drift.

Command: `conductor roster edit [--config <path>]` (sits beside the existing
`conductor roster drift`).

## Problem

The roster is 24 `[[roster]]` blocks inside a 535-line `conductor.toml`. Two
things make hand-editing it error-prone:

- `fallback` is an ordered list of *roster names*, validated at parse time.
  Removing a model orphans every chain that names it. `repo_policy` blocks
  reference roster names too.
- There is no way to take a model or a whole provider out of rotation short of
  deleting rows and losing their config.

The failure that matters is silent: disable `neuralwatt`, and three unrelated
models lose the tail of their fallback chain. Nothing tells you until dispatch.

## Decisions

| Decision | Choice |
|---|---|
| "Pull out" a model | `enabled = false` (fast, reversible) **and** a separate deliberate delete |
| "Pull out" a provider | first-class `[[provider]]` table with `enabled` |
| Terminal rendering | ratatui + crossterm, optional deps behind a default-on `tui` feature |
| TOML write-back | hand-rolled line-span editor; **no** `toml_edit` |
| Save semantics | buffer edits in memory, validate, explicit `w` to write |
| Layout | provider tree + detail/fallback pane + warning bar |

Rationale for the two dependency calls: rendering and raw-mode input are toil
with no domain value (and `unsafe_code = "forbid"` blocks doing termios
ourselves), so take ratatui. Write-back *is* domain logic — `config.rs` already
holds every validation rule — so adding `toml_edit` would put two TOML
semantics in one tree that could disagree. Keep one.

## Schema changes (`conductor.toml`)

New provider table, seeded with the 7 providers already referenced by roster
rows (`agy`, `anthropic`, `google-ai-studio`, `neuralwatt`, `ollama-cloud`,
`openai-codex`, `opencode-go`):

```toml
[[provider]]
name = "neuralwatt"
enabled = true
```

`RosterEntry` gains `enabled: bool`, optional in TOML, **defaulting to `true`**
so all 24 existing rows parse unchanged.

**Effective enablement:** a model is dispatchable iff
`roster.enabled && provider(roster.provider).enabled`.

**New hard validation, mirroring the existing fallback rule:** every roster
row's `provider` must resolve to a declared `[[provider]]` block. A typo fails
closed at `config check` rather than silently darkening a model. (All 24 rows
currently declare a provider, so nothing is grandfathered.)

## Phase 1 — dispatcher honors `enabled`

Lands **first**, on its own. A flag the dispatcher ignores is theater.

- Model selection in triage/dispatch filters to effective-enabled models.
- The fallback chain walk skips effective-disabled links.
- `conductor config check` reports enabled counts per tier.

**Open decision, owned by the user (see Contribution below):** when a model's
*primary* is effective-disabled, is it excluded from selection entirely, or
still selectable with dispatch walking immediately to its first enabled
fallback?

## Phase 2 — `src/config_edit.rs`: line-span editor

Not a TOML parser. A **line indexer**:

- Scans for `[[roster]]` / `[[provider]]` headers and `key = value` lines;
  records line spans per block and per key. Balances brackets so a multi-line
  `fallback = [...]` value stays intact as one span.
- Four splice ops: `set_key`, `remove_key`, `delete_block`, `insert_block`.
  `set_key` replaces the key's line if present, else inserts after the block's
  last key line. `insert_block` appends after the last block of that kind.
- Lines that aren't touched are never rewritten. **Comments survive by
  construction**, not by careful reproduction.

**The safety net:** before any write, the rendered buffer is run back through
`config::parse_str()` and the write is **refused** if it doesn't load. The
indexer is allowed to be dumb precisely because the real parser is the gate —
the TUI structurally cannot emit a `conductor.toml` that Conductor rejects.

**Accepted trade-off:** `delete_block` removes only the block's own lines
(header through last value line) and leaves adjacent comments alone. Deleting a
model can orphan its comment. This is cosmetic, never a correctness bug, and
delete is the rare path. The alternative — a comment-ownership heuristic —
would eat the "GPT-5.6 Codex lane" comment that describes four rows.

## Phase 3 — the TUI

`src/tui/{mod,state,view}.rs`. ratatui + crossterm as optional deps behind a
`tui` feature (default-on; the gate keeps a minimal build possible).

A panic hook restores the terminal before exit — panic hooks still run under
`panic = "abort"`, so a crash won't strand the user in raw mode.

State is a `RosterDraft`: in-memory roster + providers + dirty flag. Every edit
mutates the draft. `w` translates draft → splice ops → lines → validate → write.

### Layout

```
┌ Conductor Roster ─────────────────────┬──────────────────────────┐
│ ▾ ● anthropic              2/2 on     │ glm-5.2                  │
│      ● sonnet-5      lead   L   paid  │ ──────────────────────── │
│      ● opus-4.8      lead   XL  paid  │ tier        senior       │
│ ▾ ● opencode-go            2/3 on     │ ceiling     M            │
│      ● minimax-m3    sr     M   paid  │ efficiency  lean         │
│    ▸ ● glm-5.2       sr     M   paid  │ backend     pi           │
│      ○ qwen3.7-max   sr     M   paid  │ dispatch_id opencode-go/ │
│ ▾ ⊘ neuralwatt             0/4 OFF    │             glm-5.2      │
│      ⊘ nw-glm52      sr     M   paid  │ cost        paid         │
│      ⊘ nw-kimi-fast  sr     M   paid  │                          │
│ ▸ ● openai-codex           4/4 on     │ FALLBACK CHAIN           │
│ ▸ ● ollama-cloud           3/3 on     │  1 ollama-glm-5.2    ok  │
│                                       │  2 nw-glm-5.2-short  ⊘   │
│                                       │  3 nw-glm-5.2        ⊘   │
├───────────────────────────────────────┴──────────────────────────┤
│ ⚠ glm-5.2: 2 of 3 fallbacks unreachable (provider neuralwatt off)│
│ space toggle · f fallbacks · d delete · w write · q quit          │
└──────────────────────────────────────────────────────────────────┘
```

Models nest under collapsible providers, so the provider toggle sits directly
above the models it darkens — the blast radius is visible *before* the keypress.

### Keymap

`↑↓`/`jk` nav · `←→`/`hl` collapse/expand · `space` toggle enabled (model or
provider) · `e` cycle an enum field in the detail pane · `f` fallback editor ·
`a` add model · `d` delete (confirm) · `r` refresh bursar + drift · `w` write ·
`q` quit (prompts if dirty).

### Fallback editor (modal)

Ordered chain on one side, eligible roster names on the other. `J`/`K` reorder,
`x` remove, `enter` add. Self-reference and cycles are rejected at edit time.

### Add-model form

Free-text `name` and `dispatch_id`; picker for `provider`, `backend`, and the
enums. `reasoning_effort` is required iff `backend = "codex"` and rejected
otherwise (existing `config.rs` rule — surface it in the form, don't duplicate
the logic).

### Overlays

- **Bursar**: live provider availability next to each provider toggle, via the
  existing `BursarClient` / `StatusReport { providers: BTreeMap<String,
  ProviderStatus> }`. `CommandBursarClient` shells out, so fetch on a background
  thread at startup and on `r` — never block the event loop.
- **Drift**: per-row markers against `~/.claude/model-scorecard.md`, reusing
  `roster_drift::parse_scorecard` + `diff`. Read-only; drift is surfaced, not
  fixed.

## Validation: two levels

**Hard — blocks the write.** Anything `config::parse_str` rejects: unknown
fallback name, codex row missing `reasoning_effort`, undeclared provider.

**Soft — warns in the status bar, never blocks.** A fallback link that is
effective-disabled; a `repo_policy` still naming a disabled or deleted model; a
tier with zero enabled models. These parse fine but are operationally wrong —
exactly the class of mistake the tool exists to catch. They must not block the
write, because temporarily disabling a model while leaving its chains intact is
a legitimate, common act.

## Testing

- **Identity round-trip**: parse the real `conductor.toml`, make zero edits,
  render — assert byte-identical output.
- **Golden splices**: each op touches only the expected lines (diff-asserted).
- **Invariant**: any sequence of edit ops produces output that
  `config::parse_str` accepts.
- **Multi-line arrays**: a `fallback` spanning lines is spliced correctly.
- **Schema**: `enabled` defaults to true; undeclared provider fails; the
  effective-enabled truth table.
- **Dispatcher** (phase 1): a disabled model is never selected; the fallback
  walk skips disabled links.
- **TUI**: headless `RosterDraft` state transitions; a ratatui `TestBackend`
  snapshot of the tree render.

## Non-goals (v1)

- Editing `repo_policy`, `arena_profile`, or `budgets` — roster + providers only.
- Renaming a model in place (delete + re-add). Rename silently breaks `fallback`
  and `repo_policy` references.
- Mouse support.
- Writing back to the scorecard.

## Contribution — reserved for the user

Phase 1's model-selection rule is business logic where fleet experience beats
inference. The signature will be prepared in `dispatch`; the user writes the
body:

> When triage considers a model whose primary is effective-disabled — exclude
> it from selection entirely (predictable), or keep it selectable and have
> dispatch walk immediately to the first enabled fallback (lets a disabled
> primary act as a routing alias)?

## Landmines

- **Never run bare `cargo fmt`** in this repo — the baseline is not
  rustfmt-clean and it churns unrelated files. Scope to edited files:
  `cargo fmt -- src/<file>`.
- `conductor.toml` is the live config the fleet dispatches from. This is why
  save is buffered + validated rather than write-through.
