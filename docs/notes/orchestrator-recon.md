# Recon: backnotprop/orchestrator vs Conductor

Bead: `conductor-iz7`. Date: 2026-07-14. Reviewer: Opus (Lead).

**Subject:** <https://github.com/backnotprop/orchestrator>
**Pinned commit:** `583acf4b469b91131f96ae2136797749c788b4c7` (2026-07-09)
**License:** BUSL-1.1 · Licensor Michael Ramos · Change Date 2029-07-09 → Apache-2.0
**Size:** ~31k LoC TypeScript across `packages/{core,cli,agent}`

All line citations are against the pinned commit. Where I could not verify a
claim, it says so — per the charter, a coverage gap is reported as a gap.

---

## Why this memo exists

The 2026-07-14 comparison that spawned this bead read the README, the file
tree, and 63 ADR **titles** — not one line of source. Two of its four
headline claims turned out to be wrong. This memo is the correction, and the
reason the follow-on beads were gated behind it rather than specced from that
analysis.

---

## Q1 — Worktree isolation (ADR 0010): **REFUTED**

**Orchestrator does not implement worktree isolation. It never invokes git at all.**

The concept exists only as unconsumed type surface:

- `runtime/types.ts:30-31` — `CwdPolicy = "workspace" | "worktree" | "any"`, `IsolationDefault = "shared" | "worktree"`
- `runtime/types.ts:40` — `supportsWorktree: boolean` on the runtime descriptor
- `runtime/runtimes.ts:50,101,140,193,249,297` — every real runtime declares `supportsWorktree: true`; `shell` declares `false` (`:332`)

Nothing reads them. The only consumer anywhere in `packages/` is a bare
re-export (`runtime/index.ts:35`). A repo-wide grep for `git worktree` /
`worktree add` / any git subprocess returns **nothing**. The sole git
awareness in the codebase is walking up to find the nearest `.git` to resolve
the workspace root (`cli/src/parsing/primitives.ts:29`) — and the Codex
runtime is even launched with `--skip-git-repo-check`
(`runtime/runtimes.ts:71`).

Workers run in a **shared cwd**. ADR 0010 is a recorded intention, not a
shipped feature.

**Consequence:** Conductor's Arena worktree support (`config.rs:420`,
`arena.keep_worktrees`) is *more* than orchestrator has. There is no design to
mine here. `conductor-fia` is a build-clean item, and adopting orchestrator
buys us zero progress toward it.

## Q2 — Codex session resume / follow-up (ADRs 0048–0058): **CONFIRMED, but Codex-only**

This one is real, and it is a genuinely good piece of engineering — but it is
far narrower than the ADR titles imply.

There are **two distinct executors**:

1. **`process.ts`** — the one-shot process executor. Its handle is
   `{ completed, interrupt }` and nothing else (`tasks/executors/process.ts:399-401`).
   No `sendMessage`, no `startGoal`, no resume.
2. **`codex-app-server.ts`** — a full JSON-RPC-over-stdio client against
   Codex's own app-server protocol.

The `TaskExecutionHandle` interface makes the split explicit — `sendMessage?`,
`startGoal?`, `controlGoal?` are all **optional** members
(`tasks/executors/types.ts:44-48`).

The JSON-RPC methods orchestrator actually calls (exhaustive, grepped from
`.request(` / `.notify(` call sites in `packages/core/src`):

```
initialize            thread/start        turn/start
initialized           thread/resume       thread/unsubscribe
thread/goal/get       thread/goal/set     thread/goal/clear
account/rateLimits/read
```

**Which runtimes get which executor** (`runtime/runtimes.ts`):

| Runtime | id line | Executor | Resume / follow-up? |
|---|---|---|---|
| claude-code | `:9` | process | **no** |
| codex | `:61` | process (`codex exec`) | **no** |
| **codex-app-server** | `:112` | protocol | **yes** |
| copilot | `:154` | process | **no** |
| grok | `:206` | process | **no** |
| pi | `:261` | process | **no** |
| shell | `:308` | process | **no** |

**This is the finding that reframes the whole adoption question.** The
capability I flagged as "the single biggest gap vs dispatch.rs" exists for
exactly **one** runtime — and it is a thin client over a protocol that **Codex
itself exposes**. `thread/resume` and `turn/start` are not orchestrator's
invention; they are `codex app-server`'s API. We can speak that protocol from
Rust directly, against the `codex` binary we already depend on, with no BUSL
dependency and no Node runtime.

For Claude Code, pi, and agy — three of Conductor's four backends —
orchestrator offers **exactly what `dispatch.rs` already does**: spawn a
process, wait, kill on timeout.

## Q3 — Provider limits (ADR 0060 + limit-reader spike): **CONFIRMED — the best asset here**

Substantial and directly mineable for Bursar. Real sources, not guesses:

- **Claude** (`provider-limits/claude.ts`): hits
  `https://api.anthropic.com/api/oauth/usage` (`:13`) with beta header
  `oauth-2025-04-20` (`:14`), using OAuth credentials read from disk
  (`:2-4,57,105-107`). On credential/API failure it falls back to parsing
  `claude auth status` CLI output (`:127`, `readAuthStatusFallback`).
- **Codex** (`provider-limits/codex-auth-store.ts`, `codex-oauth.ts`): reads
  the Codex auth store from disk, refreshes tokens against
  `https://auth.openai.com/oauth/token` (`codex-auth-store.ts:5`), and reads
  live limits via the app-server's `account/rateLimits/read`
  (`provider-limits/codex-app-server.ts`). Failure modes are enumerated as
  typed cases — `auth_missing`, `auth_failed`, `oauth_tokens_missing`,
  `oauth_refresh_missing`, `oauth_refresh_failed` (`codex.ts:97-101`).
- **Copilot** (`provider-limits/copilot.ts`, `copilot-mapping.ts`).

Crucially — this answers `bursar-ejf`'s question 3 — **they model limit
*windows* with resets, not a boolean exhausted flag**
(`claude-mapping.ts:87,102,106-108`): a 5-hour session window
(`windowMinutes: 5 * 60`) and a 7-day weekly window (`7 * 24 * 60`), each
carrying utilization/percent and a reset time
(`claude-mapping.ts:196-206`). That is exactly the shape
`evaluate_budget` (`conductor/src/bursar.rs:181`) needs to make a per-provider
go/no-go call.

Bursar's spec already named the right two sources ("Anthropic OAuth usage
endpoint + Codex rate_limits") — we converged independently. What is mineable
is the **specifics**: the exact URL, the beta header, the auth-store refresh
dance, the window shape, and the typed failure taxonomy.

## Q4 — Model discovery (ADR 0063): **CONFIRMED**

Per-runtime readers (`model-discovery/{claude,codex,copilot,grok,pi}.ts`)
behind a common shape (`catalogs.ts:7`, `availableCatalog`) that records
`source`, `cliVersion`, `defaultModel`, `models[]`, and `discoveredAt`, with
an explicit `unavailableCatalog` path (`catalogs.ts:26`) and error redaction
(`redactCatalogError`). Status is a tri-state `available | partial |
unavailable` — it degrades honestly rather than pretending.

Modest but real. It is the antidote to stale model slugs in the scorecard, and
it composes with `roster_drift.rs`. Note our roster does something theirs does
not: **evidence-based tiering** from the bench log. Discovery answers "does
this model exist"; the scorecard answers "is it any good." Different jobs.

## Q5 — Task store (ADRs 0030, 0045): **CONFIRMED — and it is a Guildhall-shaped substrate**

`~/.orchestrator/tasks/<taskId>/` (`tasks/store.ts:44,52,56`, overridable via
`ORCHESTRATOR_HOME`), one directory per task, containing
(`tasks/store.ts:60-68`):

```
task.json  heartbeat.json  stdout.log  stderr.log  combined.log
events.jsonl  transcript.jsonl  result.md  artifacts/
```

This is squarely compatible with the charter's substrate principle —
**artifacts on disk are the event bus**. `events.jsonl` + `transcript.jsonl` +
`heartbeat.json` are exactly the shape a hindsight ingestion reader consumes,
and heartbeats give stale-task reconciliation (ADR 0045). If we ever run
orchestrator for anything, hindsight can read it without a new IPC mechanism
and without a charter amendment.

## Q6 — Does anything verify the work *landed*? **NO — and this is the disqualifier**

From the process executor (`tasks/executors/process.ts:257-264`):

```ts
const finalStatus =
  isTaskStopRequested(current) || cancelRequested
    ? "cancelled"
    : timedOut
      ? "timed_out"
      : code === 0 && !adapterResult.failed
        ? "succeeded"
        : "failed";
```

**`code === 0` ⇒ `"succeeded"`.** The exit code is the success oracle. The
only cross-check is `adapterResult.failed`, which is set from *parsing the
runtime's own output stream* (`tasks/output-adapters.ts:230-255`,
`:516` `turn.failed`, `:650` `exitCode === 0 ? "succeeded" : "failed"`) — i.e.
the worker's own testimony about itself.

This is precisely the trap the Guildhall charter was written around:

> **exit codes are testimony; artifacts are evidence.** agy exits 0 on
> quota-exhausted no-ops.

Orchestrator would mark an agy quota-exhausted no-op **succeeded**. ADR 0008
("do not require structured worker output in v1") is not a v1 shortcut we can
wait out — it is a stated design position, and every status the task store
reports inherits it.

`dispatch.rs` is **strictly better here**, and this inverts the framing of the
original comparison, which cast dispatch.rs as the weak half. Conductor
classifies against a `CommitProbe` — HEAD before/after (`dispatch.rs:140,483`)
— cross-checked with stdout length, which is why it has tests named
`exit_zero_with_no_new_commit_and_zero_stdout_is_backend_flake_failure` and
`exit_zero_with_no_new_commit_and_nonzero_stdout_is_no_new_commit_failure`
(`dispatch.rs:727,752`). Orchestrator has no equivalent and, by ADR 0008, no
intention of one.

## Gaps named honestly

- **No agy runtime.** Built-ins are claude-code, codex, codex-app-server,
  copilot, grok, pi, shell (`runtime/runtimes.ts`). agy *could* be added as a
  custom process runtime via `~/.orchestrator/config.json`
  (`doc/custom-agents.md:12,22-27`) — but it would inherit exit-code success
  semantics, which for agy specifically is **exactly wrong** (Q6). The one
  backend we would most want to wrap is the one the wrapper is least safe for.
- **Node/TS runtime** in a Rust binary's dispatch path. Precedent exists (we
  shell to `bd`, `bursar`), but it is a new toolchain to install and pin.
- **Maturity.** Single author, pre-1.0, 63 ADRs in ~3 weeks, BUSL-1.1.
  Fast-moving upstream on a load-bearing seam.
- **Not verified by me:** I did not run the CLI or execute their test suite.
  Every claim above is static reading of the pinned tree. The Codex
  app-server resume path in particular is *read* as working, not *observed*
  working — if `conductor-zr2` leans on it, observe it first.

---

## Recommendation input for `conductor-zr2`

Not the decision — that bead is lead-floor and owns the call. But the evidence
points hard in one direction:

**Option A (adopt as an Exec backend) has lost most of its value.** The two
capabilities that justified it are gone or narrowed: worktree isolation does
not exist (Q1), and resume/follow-up covers one runtime we could speak to
directly ourselves (Q2). What remains — a task store and process supervision —
we already have, with a **better success oracle** (Q6). Adopting would import a
BUSL dependency, a Node toolchain, and a weaker verification model, to wrap
three of four backends in a layer that does nothing extra for them.

**Option B (mine-only) is where the value is**, and it is real:

1. **Bursar** (`bursar-ejf`) — the limits detectors are the single best asset
   in the repo. Endpoints, headers, auth-store refresh, window+reset shape,
   typed failure taxonomy. High value, zero license exposure, no dependency.
2. **Codex app-server protocol, in Rust** — file this as new work. `thread/start`,
   `thread/resume`, `turn/start`, `account/rateLimits/read` are **Codex's** API,
   not orchestrator's. A native client gives Conductor session resume and
   verify-failure-feedback for the Codex backend with no BUSL, no Node, and
   `verify.rs` still holding the gavel. This is the capability worth having,
   reached the right way.
3. **Model discovery** — the tri-state degrade-honestly catalog shape is a
   modest, clean pattern for `roster_drift.rs`.

**`conductor-fia` (worktree isolation) stands on its own**, unchanged in value
but with no design to borrow. Its hard part was never the isolation — it is
teaching `verify.rs`'s `CommitProbe` to judge the right tree.

**`conductor-kfq` (OrchestratorExec spike) should probably close as
`wont-fix`** if `conductor-zr2` lands on B, and be replaced by the native
Codex app-server client above.
