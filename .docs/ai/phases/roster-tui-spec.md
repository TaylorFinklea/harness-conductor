# roster-tui — spec

Status: designed 2026-07-13. Revised 2026-07-13 after adversarial review
(opencode-go/glm-5.2). Not started.

A terminal UI for managing the Conductor model roster: toggle models and
providers in/out, edit fallback chains, add models, all against a live view of
provider health and scorecard drift.

Command: `conductor roster edit [--config <path>]` (sits beside the existing
`conductor roster drift`).

## Problem

The roster is 24 `[[roster]]` blocks inside a 535-line `conductor.toml`.
`fallback` is an ordered list of *roster names*, validated at parse time
(`config.rs:1338-1350`), so removing a model orphans every chain that names it.
And there is no way to take a model or a whole provider out of rotation short of
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
| Disabled primary | **excluded from selection entirely** (resolved; see Phase 1) |

Rationale for the two dependency calls: rendering and raw-mode input are toil
with no domain value (and `unsafe_code = "forbid"` blocks doing termios
ourselves), so take ratatui. Write-back *is* domain logic — `config.rs` holds
the validation rules — so adding `toml_edit` would put two TOML semantics in one
tree that could disagree. Keep one.

## Corrections from adversarial review

The first draft of this spec asserted three things that are **false**. They are
recorded here so the same mistakes aren't reintroduced:

1. **`repo_policy` does NOT reference roster names.** `RepoPolicy` is
   `{repo, cost_policy}` (`config.rs:230`); `parse_repo_policies`
   (`config.rs:1307-1336`) reads only those two keys, and `repo` is a git
   directory name, not a model. It takes `&[RosterEntry]` only because the
   fallback-name validation loop is (misplaced) inside it. **Deleting or
   disabling a model orphans no `repo_policy`.** The real roster↔repo_policy
   relationship is by *cost category* (`CostPolicy::allows`, `config.rs:200-209`,
   consumed at `dispatch_cycle.rs:489` and `triage.rs:202`).
2. **"`parse_str` is the gate" was circular.** The parser has no `provider` in
   its top-level allowlist (`config.rs:877-893`) and no `enabled` on
   `RosterEntry` — so today it would reject every file the TUI writes. The gate
   only exists *after* Phase 1 extends it.
3. **A splice can be parseable AND semantically wrong.** See Phase 2; this
   invalidated the original "the indexer is allowed to be dumb" argument.

## Schema changes (`conductor.toml`)

New provider table, seeded with the 7 providers already referenced by roster
rows (`agy`, `anthropic`, `google-ai-studio`, `neuralwatt`, `ollama-cloud`,
`openai-codex`, `opencode-go`):

```toml
[[provider]]
name = "neuralwatt"
enabled = true          # optional; defaults to true
```

`RosterEntry` gains `enabled: bool`, optional in TOML, **defaulting to `true`**
so all 24 existing rows parse unchanged. `Provider.enabled` is likewise optional
and defaults to `true`.

**Effective enablement:**

- `provider == ""` → **no provider gate**; effective = `roster.enabled`. (Empty
  is the current default when the key is omitted, `config.rs:1240`, and several
  test fixtures rely on it. Legacy/test-only shape.)
- `provider != ""` → must resolve to a declared `[[provider]]` block, else a
  **hard parse error** (fail closed on typos, mirroring the fallback rule).
  Effective = `roster.enabled && provider.enabled`.

## Phase 1 — schema + dispatcher, landing ATOMICALLY

The original draft called this "lands first, on its own." That was wrong: the
provider-resolution rule breaks the live `conductor.toml` the instant it lands,
because there are **zero** `[[provider]]` blocks today. All of the following
must be one commit, or the fleet's config stops parsing and
`checked_in_config_parses_and_has_phase2_roster_entries` (`config.rs:1625`)
fails:

1. **Parser extension.** Add `provider` to the `from_doc` top-level allowlist
   (`config.rs:877-893`); add a `Provider` struct + `parse_providers`; add
   `enabled` to the roster and provider key allowlists and to the structs; add
   the provider-resolution check.
2. **Seed migration.** Write the 7 `[[provider]]` blocks into `conductor.toml`.
3. **Fix the compile sites.** `RosterEntry` struct literals that must gain
   `enabled`: `config.rs:1248`, plus test helpers `roster_drift.rs:532`
   (`cfg_entry`), `verify.rs:1380`, `triage.rs:496` (`roster_entry`).
4. **Dispatcher honors effective-enabled** (below).

A flag the dispatcher ignores is theater, so 4 is not optional in this commit.

### Selection semantics — RESOLVED: exclude a disabled primary entirely

A disabled model is **never selected**, and a disabled link in a fallback chain
is **skipped**. These are the *same* rule, so there is no special case for
`chain[0]`.

Reasoning: `select_candidate` must return a model that will actually run —
selecting one that won't would write a never-executed model into the ledger. The
"routing alias" alternative (keep it selectable, walk straight to its first
enabled fallback) also contradicts what `enabled = false` means, and bursar
already provides *dynamic* per-provider deferral (`evaluate_budget` → `Defer` →
`next_eligible_roster`, `dispatch_cycle.rs:503-520`) for the soft case. Manual
`enabled` is the hard off knob; bursar is the soft one.

### Do NOT fold `enabled` into `candidate_rejection`

`candidate_rejection` (`triage.rs:183`) is shared by `select_candidate`
(`triage.rs:219`), the per-link walk check (`dispatch_cycle.rs:492`), and
`next_eligible_roster` (`dispatch_cycle.rs:680`). If `enabled` goes in there,
`select_candidate` returns `None` for an all-disabled tier and `route` pushes
**`Flag::OverCeiling`** (`triage.rs:351`) — reporting "you turned these off" as
"this item is too hard." That is a silent misattribution, and worse for a
ratchet-unlocked auto-dispatch item.

Instead: a **separate** effective-enabled predicate, applied *after*
`candidate_rejection` in `select_candidate`, and threaded into the walk and
`next_eligible_roster`. Add a distinct `Flag::AllDisabled { repo, issue_id, tier }`
for "candidates exist but all are effective-disabled." In the walk, a disabled
link is a **hard skip** (`record_fallback_skip`, like tier/ceiling/cost
rejections at `dispatch_cycle.rs:709`), *not* the bursar `Deferred` path — those
mean different things (`Deferred` = "no link ran this cycle"; skip = "this link
is ineligible").

**Touch-points:** `select_candidate` (`triage.rs:219`), `candidate_rejection`
(`triage.rs:183`, left alone), `run_worker_chain` (`dispatch_cycle.rs:461`),
`fallback_chain` (`dispatch_cycle.rs:774`, built at `:488`),
`next_eligible_roster` (`dispatch_cycle.rs:680`).

Also: `conductor config check` reports enabled counts per tier.

## Phase 2 — `src/config_edit.rs`: line-span editor

A **line indexer**, not a TOML parser: it scans for `[[roster]]` /
`[[provider]]` headers and `key = value` lines, records line spans per block and
per key, and exposes four splice ops — `set_key`, `remove_key`, `delete_block`,
`insert_block`.

### The safety argument, corrected

The original claim — "the indexer is allowed to be dumb precisely because the
real parser is the gate" — is **withdrawn**. `parse_str` gates *parseability*,
not *semantic correctness*. A splice can be both valid TOML and wrong:

> The parser accepts a trailing comment on a header: `[[roster]] # header comment`
> (`skip_inline_ws` eats `#`→EOL, `config.rs:551`; the test at `config.rs:1944`
> exercises exactly this form). An indexer matching `^\s*\[\[roster\]\]\s*$`
> **misses that header**, and attributes every following `key = value` line to
> the *previous* block. A `set_key` then splices into the wrong roster entry.
> The result is valid TOML, `parse_str` accepts it, the write succeeds — and the
> edit silently lands on a different model than the one on screen.

Two requirements follow, and both are load-bearing:

1. **The indexer must mirror the parser's tolerance exactly.** Header match is
   line-anchored (`^\s*\[\[name\]\]`) and tolerates trailing `# …` and inline
   whitespace, because the parser does. Never substring-match a header — a
   `[[roster]]` substring inside a single-line value (`dispatch_id = "x [[roster]] y"`)
   is legal string content. (Multi-line strings cannot occur: `parse_string`
   rejects newlines, `config.rs:756`.)
2. **Structural-equivalence check after every splice.** Re-parse the rendered
   buffer and assert the resulting `Config` differs from the pre-edit `Config`
   **only** in the intended field on the intended block. Not merely "it loads."
   This is the actual gate.

### Rendering invariant

Render = re-emit untouched raw lines + spliced lines. **Never reconstruct a
block from the parsed `Config`** — the parser's intermediate `Doc` uses a
`HashMap` (`config.rs:580-590`), so reconstruction loses key order and every
comment. Preserve the file's trailing newline explicitly (`str::lines()` strips
it; a naive `lines().join("\n")` breaks byte-identity).

### Op details

- `set_key` — replace the key's line if present, else insert after the block's
  last key line. **Must preserve a trailing `# …` comment on the line it
  rewrites** (the parser accepts `cost = "paid"  # paid lane`; none exist in the
  file today, so this is latent, not hypothetical).
- `insert_block` — append after the last block of that kind. **Define the
  bootstrap anchor for when zero blocks of that kind exist** — required by the
  Phase-1 `[[provider]]` seed itself. Anchor: immediately before the first
  `[[roster]]` block.
- `delete_block` — remove only the block's own lines (header → last value line),
  leaving adjacent comments alone. **Accepted trade-off:** deleting a model can
  orphan its comment. Cosmetic, never a correctness bug, and delete is the rare
  path. The alternative — a comment-ownership heuristic — would eat the "GPT-5.6
  Codex lane" comment that describes four rows.
- Multi-line arrays: `skip_blanks` (`config.rs:564`) consumes newlines mid-array,
  so a multi-line `fallback = [...]` is **legal** even though none exist today.
  The indexer must balance brackets. Comments inside an array are *not* legal
  (`skip_blanks` doesn't skip them), so that case needn't be handled.

## Phase 3 — the TUI

`src/tui/{mod,state,view}.rs`. ratatui + crossterm as optional deps behind a
`tui` feature (default-on; the gate keeps a minimal build possible). A panic hook
restores the terminal — panic hooks still run under `panic = "abort"`, so a crash
won't strand the user in raw mode.

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
provider) · `e` cycle an enum field · `f` fallback editor · `a` add model ·
`d` delete (confirm) · `r` refresh bursar + drift · `w` write · `q` quit
(prompts if dirty).

### Fallback editor (modal)

Ordered chain on one side, eligible roster names on the other. `J`/`K` reorder,
`x` remove, `enter` add. Self-reference and cycles rejected at edit time — note
this is a **new, TUI-only** invariant: neither `parse_str` nor `fallback_chain`
(`dispatch_cycle.rs:774`) detects cycles today, so a hand-edited config can
still contain one. Consider a parse-time cycle check so the invariant doesn't
depend on edits going through the TUI.

### Add-model form

Free-text `name` and `dispatch_id`; picker for `provider`, `backend`, and the
enums. `reasoning_effort` is required iff `backend = "codex"` and rejected
otherwise — surface the existing `config.rs` rule, don't duplicate the logic.

### Overlays

- **Bursar.** Live provider availability next to each provider toggle. **The
  lookup key is NOT `[[provider]].name`.** `bursar_provider_for`
  (`dispatch_cycle.rs:633`) rewrites `openai-codex` → `codex`
  (`dispatch_cycle.rs:645`) before querying, and `StatusReport.providers` is
  keyed by the rewritten name. Make `bursar_provider_for` `pub(crate)` and reuse
  it; duplicating the mapping would show "unknown" for the entire sol/terra/luna
  lane. `CommandBursarClient` shells out, so fetch on a background thread at
  startup and on `r` — never block the event loop.
- **Drift.** Per-row markers against `~/.claude/model-scorecard.md`, reusing
  `roster_drift::parse_scorecard` + `diff`. Read-only; drift is surfaced, not
  fixed.

## Validation: two levels

**Hard — blocks the write.** Anything `config::parse_str` rejects (unknown
fallback name, codex row missing `reasoning_effort`, unresolvable provider),
**plus** the structural-equivalence check from Phase 2.

**Soft — warns in the status bar, never blocks.** A fallback link that is
effective-disabled; a tier with zero enabled models; and — per the corrected
roster↔repo_policy relationship — **cost-category coverage**: "no enabled model
satisfies repo X's `cost_policy`." (NOT "a repo_policy names a disabled model";
that relationship does not exist.) These parse fine but are operationally wrong.
They must not block the write: temporarily disabling a model while leaving its
chains intact is legitimate and common.

## Testing

- **Identity round-trip**: parse the real `conductor.toml`, zero edits, render —
  byte-identical, trailing newline included.
- **Structural equivalence**: every splice changes exactly the intended field of
  the intended block in the parsed `Config`, and nothing else.
- **Adversarial indexer inputs**: header with trailing comment
  (`[[roster]] # note`); value line with trailing comment; a `[[roster]]`
  substring inside a string value; a multi-line `fallback` array. Each must
  splice correctly — these are the forms the parser accepts, so the indexer must
  too.
- **Schema**: `enabled` defaults true (roster and provider); empty `provider`
  bypasses the gate; non-empty unresolvable `provider` is a hard error; the
  effective-enabled truth table.
- **Dispatcher**: a disabled model is never selected; the walk skips disabled
  links via `record_fallback_skip`, not `Deferred`; an all-disabled qualifying
  tier yields `Flag::AllDisabled`, **not** `Flag::OverCeiling`.
- **TUI**: headless `RosterDraft` state transitions; ratatui `TestBackend`
  snapshot of the tree.

## Non-goals (v1)

- Editing `repo_policy`, `arena_profile`, or `budgets` — roster + providers only.
- Renaming a model in place (delete + re-add). Rename silently breaks `fallback`
  references. (It does *not* break `repo_policy` — see Corrections.)
- Mouse support.
- Writing back to the scorecard.

## Contribution — reserved for the user

The axis-5 question is resolved (exclude-entirely). What remains is genuinely
operational, and only the fleet operator can answer it:

> When `Flag::AllDisabled` fires — an item qualifies, but every model at its
> tier is effective-disabled — what should the cycle DO? Skip the item silently
> and report it? Escalate to the next tier up (which may cost real money the
> operator was trying to avoid by disabling the lane)? Or hard-fail the cycle so
> the operator notices they've darkened a tier they still depend on?

The signature will be prepared in `triage`; the body is yours.

## Landmines

- **Never run bare `cargo fmt`** — the baseline is not rustfmt-clean and it
  churns unrelated files. Scope it: `cargo fmt -- src/<file>`.
- `conductor.toml` is the live config the fleet dispatches from. This is why
  save is buffered + validated rather than write-through, and why Phase 1 must
  be atomic.
