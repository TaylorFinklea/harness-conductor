# Conductor v1 — spec

**Status**: approved design (user, 2026-07-01) — brainstormed with Fable 5, implemented by the fleet.
**Repo**: `~/git/harness-conductor` (binary name: `conductor`). The design prompt's `~/git/conductor` path does not exist; this repo is the home.
**Runtime**: Rust (user-delegated decision; rationale in decisions.md 2026-07-01).

## Mission

A single Rust binary that runs one **cycle** over the fleet of beads-tracked repos under `~/git`:
scan → triage → plan → (approval) → dispatch → verify → report. It composes existing components
over subprocess/file contracts; it owns only routing, gates, budgets, serialization, and state.
No daemon, no new database, no network listener.

## Approved product decisions (locked — do not re-litigate)

| Decision | Value |
|---|---|
| Autonomy | **Ratchet**: propose-only until a repo earns auto-dispatch (see Ratchet) |
| Trigger | Manual CLI only (`conductor cycle`); schedulers are just callers |
| Runtime | Rust, larkline-style discipline (see Precedents) |
| Budgets (defaults, all config knobs) | ≤8 dispatches/cycle, ≤1 active per repo, ≤4 to metered external backends (pi+agy combined), 45 min wall-clock/item, 90 min/cycle |
| bd routing-field writes | Conductor may `bd set-metadata` **only after human approval** of triage suggestions in the cycle report |
| Out of scope v1 | hermes-voice (v2 notification channel at most), larkline (display comes free via harness-deck), bd `swarm`/`gate`/`mol` (post-v1 spike), wrapping ralph, decomposing beads, any push to remotes |

## Ground truth (recon 2026-07-01 — component contracts implementers must code against)

### bd (beads) — the queue
- `bd -C <repo> <cmd> --json` works for nearly everything; **never `cd`**. Always redirect stdin: `< /dev/null`.
- Ready queue: `bd -C <repo> ready --json` → array of issues with fields
  `id, title, description, acceptance_criteria, notes, status, priority, issue_type, assignee, owner, created_at, updated_at, started_at, labels, dependency_count, dependent_count, comment_count` (+ `metadata` only when set, + `parent`, `dependencies` on some).
- **Two distinct zero-states** (plain-text mode): "No open issues" (drained) vs "No ready work found (all issues have blocking dependencies)" (blocked). `--json` mode likely renders both as `[]` (verify at implementation) — distinguish via `bd -C <repo> count --json` / `bd blocked --json`. Fleet-health reporting MUST distinguish them.
- Routing fields today: prose in `notes` (`"tier_floor: senior · complexity: S-M · verify_type: …"`), present on only ~8 issues fleet-wide (tesela `ra7.*`/`mp0.*`). bd's **structured metadata** (`--metadata`, `--set-metadata`, `--has-metadata-key`, `--metadata-field`, queryable) is unused and is the target home.
- Claims: `bd -C <repo> update <id> --claim` (documented atomic + idempotent-per-claimer). Release = `bd update <id> --status open --assignee ""` (note: `started_at` cannot be cleared — cosmetic residue, ignore).
- **`bd ready --claim` is a mutation** — never call it. Verified the hard way during recon.
- `--readonly` global flag exists ("block write operations, for worker sandboxes") — verify it actually enforces, then use it in any context where a worker could touch bd.
- Storage: Dolt-embedded (`.beads/metadata.json` has `"backend":"dolt"`); single-writer lock at `.beads/embeddeddolt/.lock`. Detect beads repos by `.beads/metadata.json` existence, NOT by db filename. Do not run concurrent bd writes against one repo (Conductor serializes per repo anyway).
- Errors: bogus id → non-zero exit + JSON error on `--json`. `bd context` needs a git repo; `ready`/`show`/`list` don't.

### Fleet shape (as of 2026-07-01)
- 24 of 26 dirs under `~/git` have `.beads/`; ~231 ready items. chezmoi-config has none (deliberate; **hard-excluded** anyway). harnessdeck-site is a zero-commit repo (unborn HEAD) — scanner must not crash on it.
- `.beads/last-touched` mtime is the freshness signal (git commit dates cluster fleet-wide; don't use them for dormancy).

### Dispatch backends (subprocess idioms — encode as constants + tests)
- **pi** (glm-5.2, minimax-m3, qwen3.7-max, gpt-5.5): `pi --model <dispatch_id> --thinking xhigh --approve -p '<prompt>' < /dev/null`. minimax ignores `--thinking` (harmless).
- **agy** (gemini-3.5-flash): `agy -p '<prompt>' --add-dir <repo> --model 'Gemini 3.5 Flash (High)' --dangerously-skip-permissions < /dev/null`. **`--add-dir` is load-bearing** — agy is project-scoped, not cwd-scoped; omitting it runs against the wrong repo.
- **claude** (sonnet-5, opus-4.8): `claude -p '<prompt>' --model <id>` run with cwd = repo, stdin `< /dev/null`.
- All backends: stdin closed ALWAYS (TUI-hang landmine), spawn with cwd = target repo, capture stdout+stderr to log files, timeout → SIGTERM, 3s grace, SIGKILL (mirror orchestra's pattern, driver.ts:276-281).
- Reference implementation of these idioms: `~/.local/bin/ralph:148-161`. Read it; mirror it; do not wrap ralph (it is Plan-file-scoped, not bead-scoped, and its exit codes are ambiguous).

### orchestra — the verify oracle
- `orchestra verify "<claim>" --evidence "<cmd>" --model <M> --cwd <repo>` → exit 0 pass / 1 fail / 2 error. **Always pass `--model`**: the built-in default (`opencode-go/kimi-k2.7-code`, cli.ts:13) is a de-rostered model. Default judge for Conductor: `opencode-go/qwen3.7-max` (strongest cheap auditor on record).
- No `--json` output — parse exit code; stdout line `[PASS|FAIL] (confidence N) …` is informational.
- **Exit 2 conflates** "usage error" with "pi endpoint wedged/timeout". Distinguish via stderr: `usage:` prefix vs `opencode-go endpoint likely wedged`. Wedged → retryable with backoff; usage → bug, fail the cycle step.
- Fail-closed by design: invalid/low-confidence verdicts are FAIL. Requires `bun` on PATH (wrapper exits 127 without it — preflight check).

### harness-deck — the review surface
- Publish = **atomic file write** (temp + rename) of `report.json` to `~/.harness/reports/conductor/<run-id>/report.json`. No HTTP publish API. `run-id` charset: `^[a-zA-Z0-9._-]+$`, ≤200 chars.
- Minimal manifest: `{schema:"harness-deck/report@1", id, project:"conductor", harness:"conductor", title, status:"awaiting-review", created:<RFC3339>, blocks:[…]}`. Full example: `~/git/harness-deck/samples/postgres-audit.report.json`; strict-check with `harness-deck validate <file>` before writing (or in tests).
- Interactive blocks: `approval` `{id, prompt}` → user answer lands in `responses.json` **beside** report.json, shape `{responses:{"<block-id>":{value:"approved"|"changes-requested", note, at, …}}}`. Absent file = unanswered (not an error). Poll by mtime/`at` watermark; SSE `/events` exists but is optional (local server is HTTPS via Tailscale cert — don't assume plain HTTP).
- Live heartbeat: patch the manifest's `live` object (`{updated, step, elapsed_ms, tokens, cost_usd, progress}`) every few seconds while dispatching; dashboard (and larkline's lark-plug-hdeck "In Flight" view) drops liveness after 60s.
- Status lifecycle enum: `draft | awaiting-review | answered | done`.

### Ledger
- Append one row per dispatch to `~/.claude/model-bench.jsonl`. **Read the existing file first and mirror its exact row shape** (fields include `model, role, score_1_5, verify_passed, complexity, project, notes`). Conductor sets `verify_passed` and omits `score_1_5` (humans/leads score later). The daily digest generator consumes this — do not invent a new log.

## Invariants (encode as the triage core's test suite — write these tests FIRST)

1. **Closed roster.** Only models present in `conductor.toml`'s roster receive work. Unknown model anywhere → error, never fallback.
2. **tier_floor is a hard gate.** An item routes only to a model whose tier ≥ floor. Unknown/unparseable floor → flag, never guess.
3. **Fail closed everywhere.** No runnable `verify_cmd` → item is not dispatchable (flag for triage). Verify fails → bead stays open, claim released, failure noted. Ambiguity → escalate to report.
4. **One writer per repo.** Max one active dispatch per repo per cycle; a repo with ANY pre-existing `in_progress` bead is skipped entirely (a human/agent may be mid-work).
5. **Never push. Never `chezmoi apply`. Never scan/dispatch chezmoi-config** (hard-coded deny in addition to config `exclude`).
6. **Close only verified.** `bd close` fires only after ALL of: worker process exited on its own (not timeout-killed) AND ≥1 new commit exists in the repo AND `verify_cmd` exits 0 AND (when configured) `orchestra verify` passes.
7. **Budgets are ceilings, not targets.** Hitting any budget stops planning/dispatching; remainder is reported as skipped-with-reason. Each budget gates only the items that would breach it — the external cap skips external (pi/agy) backends only, so an internal (claude) item still dispatches after the external cap is hit.
8. **No silent drops.** Every ready item the cycle saw appears in the report as dispatched / proposed / flagged / skipped(reason).
9. **Ratchet failure re-locks.** Any rejected proposal or failed verify resets that repo's counter to 0 and returns it to propose-only.

## Architecture (thin composer)

Single crate, modules with one purpose each; the triage core is pure (no IO) so it table-drives:

```
src/
  main.rs / cli.rs      — subcommand parsing, exit codes (0 ok; 1 cycle had flags/failures; 2 config/env error)
  config.rs             — conductor.toml load + validation (incl. roster)
  scan.rs               — fleet enumeration (walk ~/git, .beads/metadata.json detection, exclusions, unborn-HEAD safe)
  bd.rs                 — bd subprocess client behind a trait (BdClient) so tests use fixtures
  fields.rs             — routing-field extraction: metadata first, notes-prose fallback (pure)
  triage.rs             — routing algorithm + gates + budgets (pure — the invariant test suite lives here)
  plan.rs               — cycle plan build/serialize (~/.local/state/conductor/plans/<cycle-id>.json)
  dispatch.rs           — backend runners (pi/agy/claude) behind a trait (Exec) + timeout/kill
  verify.rs             — verify_cmd runner + orchestra subprocess + close/release decisions
  deck.rs               — report.json writer (atomic), responses.json reader, live patcher
  ledger.rs             — model-bench.jsonl appender
  state.rs / ratchet.rs — journal, ratchet counters (~/.local/state/conductor/)
```

### Routing-field extraction (`fields.rs` — pin exactly)
1. Prefer bd metadata keys: `tier_floor` ∈ {`lead`,`senior`,`junior`}, `complexity` ∈ {`S`,`M`,`L`,`XL`}, `verify_cmd` = exact shell command.
2. Fallback: parse `notes` with case-insensitive regexes `tier_floor:\s*(lead|senior|junior)` and `complexity:\s*(XL|S|M|L)(?:\s*[-–]\s*(XL|S|M|L))?` — **a range like `S-M` resolves to its upper bound**. A notes `verify_type:` prose line is NOT a runnable verify_cmd; it only informs triage suggestions.
3. Anything missing/unparseable → the item is `Untriaged` and can only be dispatched as a triage-suggestion target (M5), never as work.

### Routing algorithm (`triage.rs` — pin exactly)
Complexity order `S<M<L<XL`; tier order `junior<senior<lead`; efficiency order `lean<std<heavy`.
1. Drop repos: excluded, any `in_progress` bead present, or repo already used this cycle.
2. For each ready item with complete fields: candidate models = roster where `tier ≥ tier_floor` and `ceiling ≥ complexity`, grouped by tier; take the **lowest qualifying tier**, then most efficient; tie → fewer dispatches so far this cycle; then roster order.
3. No candidate (complexity above every qualifying ceiling) → flag `over-ceiling` for the user.
4. Apply budgets in priority order (bd priority asc, then oldest `created_at`): stop at any ceiling; excess → `skipped(budget)`.
5. Lead-floor items are ALWAYS propose-only (never auto-dispatched by ratchet).

### Roster (`conductor.toml` — initial contents, from scorecard 2026-07-01)

| name | tier | ceiling | efficiency | backend | dispatch_id |
|---|---|---|---|---|---|
| sonnet-5 | lead | L | std | claude | claude-sonnet-5 |
| opus-4.8 | lead | XL | heavy | claude | claude-opus-4-8 |
| gpt-5.5 | senior | M | std | pi | openai-codex/gpt-5.5 |
| minimax-m3 | senior | M | lean | pi | opencode-go/minimax-m3 |
| qwen3.7-max | senior | M | lean | pi | opencode-go/qwen3.7-max |
| glm-5.2 | senior | M | lean | pi | opencode-go/glm-5.2 |
| gemini-3.5-flash | junior | S | lean | agy | Gemini 3.5 Flash (High) |

(sonnet-5's scorecard "XL via decomposition" is capped to L here because decomposition is out of scope v1; XL lead items therefore route to opus-4.8 or get flagged.)
`conductor roster drift` parses the Live Roster table in `~/.claude/model-scorecard.md` and warns (never auto-edits) when it disagrees with conductor.toml. Config is truth at dispatch time; the scorecard is the human-maintained upstream.

### Dispatch worker contract (`dispatch.rs`)
- Conductor claims the bead (`bd update --claim`) BEFORE spawning; on worker failure it releases (`--status open --assignee ""`) and comments (`bd comment <id> "conductor: <cycle> <model> failed: <summary>"`).
- Worker prompt template (checked into repo, `templates/worker-prompt.md`) contains: bead id/title/description/acceptance_criteria/notes verbatim; repo path; the rules — read files before editing, stay in scope, ONE commit, run `<verify_cmd>` yourself before finishing, NEVER push, NEVER touch bd, NEVER `chezmoi apply`, do not close/claim anything.
- Workers never run bd. All bd writes are Conductor's.
- Logs land in `~/.local/state/conductor/logs/<cycle>/<bead>.{out,err}`.

### Verify pipeline (`verify.rs`)
1. Worker exit + new-commit check (`git -C <repo> rev-parse HEAD` before/after; no new commit → fail, release).
2. Run `verify_cmd` in repo, capture exit code (this alone can pass an item).
3. If item is tagged `adversarial: true` in metadata OR config `verify.always_orchestra = true`: `orchestra verify "<title>: <acceptance_criteria>" --evidence "<verify_cmd>" --model <config.verify.judge> --cwd <repo>`; exit 0 required.
4. Pass → `bd close <id> --reason "conductor <cycle>: verified via <verify_cmd>"` + ledger row (`verify_passed: true`). Fail → release + comment + ledger row (`verify_passed: false`) + report entry. Worker commits are LEFT IN PLACE either way (human reviews; conductor never reverts).

### Cycle report (`deck.rs`)
One report per cycle, run-id `cycle-YYYYMMDD-HHMMSS`. Blocks: `metrics` (repos scanned, ready, triaged %, dispatched, verified, flagged), `table` (per-repo queue + the two zero-states distinguished), `approval` id=`dispatch-plan` (the proposed dispatches: bead → model → why), `approval` id=`triage-backfill` (M5: proposed tier_floor/complexity/verify_cmd values), `callout` (escalations: over-ceiling, missing fields, roster drift, budget skips). `conductor dispatch <cycle-id>` refuses unless `responses.json` shows `dispatch-plan: approved` (fail-closed; `changes-requested` → cycle closed, nothing runs).

### Ratchet (`ratchet.rs`)
- State: `~/.local/state/conductor/ratchet.json` — `{repo: {clean_cycles: N, unlocked: bool}}`.
- A cycle is *clean for a repo* iff every proposal touching it was approved unmodified AND every dispatch in it verified-closed. 3 consecutive clean cycles → unlocked.
- Unlocked repos: `conductor cycle` may auto-dispatch items with `tier_floor ∈ {senior,junior}` AND `complexity ≤ M` AND a runnable `verify_cmd`, within budgets, WITHOUT waiting for approval — but they still appear in the report. Everything else still proposes.
- Any rejection, verify failure, or worker failure in a repo → `clean_cycles = 0`, `unlocked = false`.
- Global override: `autonomy = "propose"` in conductor.toml disables auto everywhere.

## Milestones (each independently shippable; one bead-set each)

- **M0 — bootstrap**: cargo skeleton mirroring larkline's discipline, conductor.toml + config.rs + `conductor config check` (preflight: bd/pi/agy/claude/orchestra/bun/harness-deck on PATH, state dir writable). Verify: `cargo test && cargo clippy -- -D warnings`.
- **M1 — scan/status**: `conductor scan [--json]`, `conductor status`. Fleet table with the two zero-states, `.beads/last-touched` freshness, unborn-HEAD safety, exclusions. Verify: `cargo test` + run `conductor scan` against the live fleet, spot-check against `bd -C ~/git/tesela ready --json | jq length` — counts match.
- **M2 — triage core**: fields.rs + triage.rs, pure, with the invariant test suite (table-driven fixture backlogs covering every invariant above, incl. range-complexity, unknown-floor flag, budget ceilings, in_progress skip). Verify: `cargo test`.
- **M3 — dry-run cycle** (the prompt's shippable milestone): `conductor cycle --dry-run` publishes a full report to harness-deck (no approval block action yet — plan is informational), validated with `harness-deck validate`. Verify: `cargo test` + report renders on the local dashboard (human check).
- **M4 — dispatch + verify**: plan approval round-trip, claims, backend runners, verify pipeline, ledger, live heartbeats. Verify: `cargo test` + one end-to-end dispatch of a synthetic S-complexity bead in a sandbox repo created by the test (NOT a real fleet repo).
- **M5 — triage backfill**: dispatch a lead-tier model (sonnet-5) to *suggest* fields for untriaged ready items (read-only worker, produces JSON suggestions); report approval block; on approval `bd set-metadata` writes them. Verify: `cargo test` + end-to-end against the sandbox repo.
- **M6 — ratchet**: counters, unlock/relock, auto-dispatch path. Verify: `cargo test` (simulated cycle sequences).

## Precedents to read before implementing (codebase-derived — mirror, don't guess)

- `~/git/larkline/Cargo.toml` — release profile, lint policy (`unsafe_code = "forbid"`), dep discipline. Mirror it.
- `~/.local/bin/ralph:148-161` — backend invocation idioms (the ONLY correct pi/agy/claude argv shapes).
- `~/.local/lib/orchestra/driver.ts:269-294` — subprocess timeout/SIGTERM/SIGKILL + wedged-endpoint detection.
- `~/git/harness-deck/samples/postgres-audit.report.json` + `hdeck contract` — manifest shapes.
- `~/.claude/model-bench.jsonl` — exact ledger row shape.
- `~/git/harness-deck/internal/beads/` (branch `feat/beads-backlog-viewer`, unmerged) — a working Go `bd --json` client to sanity-check bd parsing against. Do NOT depend on its HTTP API (unshipped).

## Deferred / non-goals (v1)

bd `swarm`/`gate`/`mol` orchestration primitives; hermes-voice event channel; SSE-based response push; launchd scheduling; decomposing XL items; multi-machine; parsing scorecard as live roster; `--json` mode for orchestra (upstream nicety, not required); `bd dolt start` server mode (embedded + per-repo serialization suffices).
