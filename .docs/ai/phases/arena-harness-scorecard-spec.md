# Arena harness scorecard — spec

**Status**: approved design (user, 2026-07-04)
**Owning repo**: `~/git/harness-conductor` for Arena instrumentation.
**Cross-repo source**: `~/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs` for the evergreen harness-deck reports.

## Goal

Arena comparisons should leave durable routing evidence in harness-deck at two levels:

1. **Model scorecard** stays model-focused and continues to ingest Arena candidate rows from `~/.claude/model-bench.jsonl`.
2. **New harness scorecard** focuses on harness/profile behavior: which harness/profile succeeds, wins, uses fewer tokens, finishes faster, and eventually costs less.

The output should be deterministic, regenerated from the ledger, and visible as a persistent harness-deck report like `model-scorecard/digest`.

## Approved product decisions

| Decision | Value |
|---|---|
| Report surface | Add a new evergreen harness-deck report at `~/.harness/reports/harness-scorecard/digest/report.json`. |
| Model scorecard | Keep `model-scorecard/digest` and continue counting Arena rows so models receive credit. |
| Source of truth | `~/.claude/model-bench.jsonl` remains the mechanical ledger. No new database. |
| Generator | Extend `gen-scorecard-digest.mjs` to emit both reports in one deterministic run. |
| Conductor instrumentation | Future Arena rows carry run id, winner/apply flags, elapsed time, best-effort token count, and nullable future cost. |
| Cost metrics | Never invent prices. Show cost-efficiency only when `cost_usd` exists; otherwise render a clear insufficient-data callout. |
| Freshness | After Arena appends ledger rows, Conductor best-effort invokes `node ~/.local/lib/scorecard/gen-scorecard-digest.mjs`; warn on failure, do not fail the Arena. |
| Chezmoi | Edit source only. Do **not** run `chezmoi apply`; the live generator updates only after human apply or direct current-file edit by the user. |

## Existing state

- `harness-conductor/src/arena.rs` appends one ledger row per Arena candidate through `append_ledger_rows()`.
- Current Arena ledger rows already include: `date`, `model`, `harness`, `profile`, `role: "arena-candidate"`, `task`, `score_1_5`, `blind_rank`, `judge`, `verify_passed`, `complexity`, `project`, `bias_note`, `notes`.
- Current `notes` embeds the Arena run id: `conductor arena <arena-run-id> profile=<profile> reason=<reason>`.
- `gen-scorecard-digest.mjs` already writes `model-scorecard/digest` and includes a small `Harness/profile arena results — mechanical` table inside it.
- No separate harness-focused report exists.
- Ledger rows do **not** currently include `duration_ms`, `tokens_used`, or `cost_usd`.
- Some Ralph logs end with:

```text
tokens used
309,466
```

  Treat this as best-effort data; not every harness/profile emits it.

## Data model additions

Extend `LedgerRow` with optional fields. Existing rows remain valid.

| Field | Type | Populated by | Meaning |
|---|---|---|---|
| `arena_run_id` | string | Arena | Stable run id, e.g. `arena-20260704-225738-warden-vy1`. |
| `winner` | bool | Arena | True for the selected winning profile, if any. |
| `applied` | bool | Arena | True when the winner was applied to the real repo. Usually same for all rows in a run. |
| `failure_reason` | string | Arena | Candidate failure/disqualification reason; empty/omitted for clean candidates. |
| `duration_ms` | integer | Arena | Total candidate elapsed wall time: setup + Ralph + verify + probes. |
| `ralph_duration_ms` | integer | Arena | Ralph subprocess wall time only. |
| `verify_duration_ms` | integer | Arena | `verify_cmd` wall time only. |
| `tokens_used` | integer | Arena | Best-effort parsed token count from Ralph stderr. Omitted when unavailable. |
| `cost_usd` | string | future | Exact provider-reported decimal USD cost, e.g. `"0.42"`. Omitted until real data exists. |

Historical rows: the generator parses `arena_run_id` and `failure_reason` from `notes` when the new fields are absent.

## Report: `harness-scorecard/digest`

Top-level manifest:

```jsonc
{
  "schema": "harness-deck/report@1",
  "id": "digest",
  "project": "harness-scorecard",
  "harness": "conductor",
  "agent": "conductor-arena",
  "kind": "report",
  "title": "Arena Harness Scorecard — which harness wins",
  "scope": "Conductor Arena",
  "status": "done"
}
```

Blocks:

1. **Prose summary**
   - Ledger source.
   - Row/run counts.
   - Explanation that cost metrics are real-only and may be absent.

2. **Metrics block**
   - Arena runs.
   - Candidate rows.
   - Verified candidates.
   - Verified rate.
   - Winner-applied rate.
   - Rows with token data.
   - Rows with cost data.

3. **Overall harness ranking** (`html` block)
   - Group by `harness` (`pi`, `opencode`, `codex`, `claude`, etc.).
   - Columns:
     - candidates
     - distinct runs
     - verified rate
     - winner count / win rate
     - applied winner count
     - average score
     - average blind rank (lower is better)
     - median duration
     - median tokens
     - token efficiency = median `tokens_used` among verified rows, lower is better
     - cost efficiency = cost per verified success when cost data exists; otherwise `—`

4. **Harness/profile ranking**
   - Group by exact `harness / profile`.
   - Same metrics as overall harness, plus model set and project set.

5. **Recent Arena runs**
   - One row per run id, newest first.
   - Columns: run id, date, project, bead/task, candidates, verified count, winner, applied, best score, judges, note.

6. **Per-run candidate stats**
   - Candidate-level evidence table.
   - Columns: run, date, project, task, harness, profile, model, verified, score, rank, winner, elapsed, tokens, cost, reason.
   - Keep the report readable: show every candidate row when there are ≤200 Arena candidate rows; otherwise show the newest 200 rows and add a data-quality note with the omitted count.

7. **Best-by-metric callouts / leaderboard**
   - Highest verified rate.
   - Highest average judge score.
   - Best average blind rank.
   - Fastest median duration (if data exists).
   - Lowest median tokens among verified rows (if data exists).
   - Best cost per verified success (if cost exists).

8. **Data-quality callout**
   - Show counts of rows missing token/cost/duration.
   - State which rankings are unavailable or provisional.

## Ranking rules

- **Success rate** = `verify_passed === true` candidates / candidates.
- **Win rate** = rows where `winner === true` / distinct candidate rows. For old rows with no `winner`, infer winner only when a row has `blind_rank === 1` and the run has a unique first-place row; otherwise leave uncounted.
- **Applied winner rate** = runs with any `winner && applied` / runs.
- **Average score** ignores rows without `score_1_5`.
- **Average rank** ignores rows without `blind_rank`; lower is better.
- **Token efficiency** uses median `tokens_used` over verified rows only; lower is better. Do not compare groups with fewer than two token-bearing rows without a low-sample marker.
- **Duration efficiency** uses median `duration_ms` over all candidate rows with duration; lower is better. Also expose a separate verified-only median duration when at least two verified rows in the group have duration data.
- **Cost efficiency** uses total `cost_usd` / verified successes when `cost_usd` exists. If no real cost data, render `—` and a callout.

## Conductor implementation notes

Instrumentation seam: `arena.rs::run_one_candidate()`.

- Add metadata fields to `CandidateRun` rather than overloading `CandidateSummary`.
- Capture an `Instant` before candidate setup and compute total `duration_ms` just before returning.
- Wrap the Ralph subprocess call with its own `Instant` for `ralph_duration_ms`.
- Wrap `verify_cmd` with its own `Instant` for `verify_duration_ms`.
- After Ralph exits, read the per-profile `.ralph.err` log and parse the first `tokens used` marker followed by a numeric line. Strip commas. If absent/unparseable, omit `tokens_used`.
- Do not make missing token data a candidate failure.
- `append_ledger_rows()` should take `decision` and `applied` so it can populate `winner` and `applied`.
- Keep ledger append fail behavior as today: if appending ledger rows fails, the Arena fails. Digest regeneration is best-effort after append/report write.

Digest refresh seam: after ledger append and report write in `arena::run()`.

- Best-effort command: `node ~/.local/lib/scorecard/gen-scorecard-digest.mjs`.
- Skip silently or warn if the script path is missing.
- Warn, do not fail, if Node/generator exits non-zero.
- Use `stdin(Stdio::null())`.

## Generator implementation notes

File: `chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs`.

- Keep the existing model report behavior.
- Refactor shared helpers only as needed: `writeReport(project, id, report)`, `median`, `pct`, `formatMs`, `formatTokens`, `arenaRunId(row)`, `failureReason(row)`.
- Filter Arena rows with `role === "arena-candidate"`.
- Build:
  - `arenaRows`
  - `arenaRuns` grouped by run id
  - `harnessAgg` grouped by `harness`
  - `profileAgg` grouped by `harness / profile`
- Historical compatibility:
  - `arenaRunId(row)` uses `row.arena_run_id || /conductor arena (\S+)/` from `notes`.
  - `failureReason(row)` uses `row.failure_reason || / reason=(.*)$/` from `notes`.
  - `winner` is trusted if present; old-row inference is conservative.
- Validate both generated reports with `harness-deck validate` or `hdeck validate` when available.

## Verification

Minimum verification for implementation:

1. `node /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs`
2. `hdeck validate ~/.harness/reports/model-scorecard/digest/report.json`
3. `hdeck validate ~/.harness/reports/harness-scorecard/digest/report.json`
4. `cargo test arena` in `~/git/harness-conductor` after Conductor instrumentation lands.
5. A synthetic/temp ledger test for the generator covering:
   - old Arena rows with only `notes`
   - new rows with `arena_run_id`, `duration_ms`, `tokens_used`, `winner`, `applied`
   - no-cost rows rendering cost as unavailable

## Constraints / landmines

- `harness-conductor` currently has unrelated dirty Rust WIP (`src/fields.rs`, `src/roster_drift.rs`, `src/triage.rs`, `src/verify.rs`). Do not mix implementation changes into that WIP without explicitly reviewing and staging only intended files.
- `chezmoi-config` currently has unrelated docs WIP for roster fallback/free model work. Do not rewrite that state.
- Do not run `chezmoi apply`; live HOME update stays human.
- Do not invent dollar costs. Token/duration efficiency is useful now; cost efficiency waits for real `cost_usd` data.
- Arena reports may exit non-zero by design when no unique safe winner exists. Digest refresh should still happen after rows/report are written.
