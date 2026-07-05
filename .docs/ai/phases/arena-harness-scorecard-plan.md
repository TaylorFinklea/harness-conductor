# Arena Harness Scorecard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a persistent harness-deck Arena harness scorecard and instrument future Arena rows with timing/token metadata.

**Architecture:** Keep `~/.claude/model-bench.jsonl` as the only mechanical ledger. Extend the existing scorecard generator to emit both `model-scorecard/digest` and `harness-scorecard/digest`. Extend Conductor Arena candidate rows with optional metadata; old rows remain readable through notes parsing.

**Tech Stack:** Rust std/serde/chrono in `harness-conductor`; Node ESM stdlib (`node:test`, `assert`, `fs`, `os`, `path`, `child_process`) in the chezmoi scorecard generator; harness-deck report schema `harness-deck/report@1`.

## Global Constraints

- Do not run `chezmoi apply`; edit chezmoi source only.
- Do not invent dollar costs. `cost_usd` is real-only and nullable.
- Keep `model-scorecard/digest` behavior intact while adding `harness-scorecard/digest`.
- Historical Arena rows with only `notes` must still appear in the harness report.
- Arena digest regeneration is best-effort: warn on failure, never fail the Arena result.
- Stage/commit only intended files; do not mix unrelated WIP from either repo.

---

## File Structure

### `~/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs`

Responsibilities after this work:
- Read `~/.claude/model-bench.jsonl`.
- Generate the existing `model-scorecard/digest` report.
- Generate the new `harness-scorecard/digest` report.
- Provide small pure helpers for report writing, medians, percentages, Arena row compatibility, and formatting.

### `~/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs`

New test file. Responsibilities:
- Run the generator under a temp `HOME`.
- Seed a synthetic `model-bench.jsonl` with old-style and new-style Arena rows.
- Assert both model and harness reports are written.
- Assert the harness report includes old-row compatibility, new instrumentation fields, no-cost callout, and candidate/ranking data.

### `~/git/harness-conductor/src/ledger.rs`

Responsibilities after this work:
- Keep existing dispatch row shape.
- Add optional serialized fields for Arena metadata.
- Test that rows omit absent fields and include present Arena metadata.

### `~/git/harness-conductor/src/arena.rs`

Responsibilities after this work:
- Capture candidate elapsed times.
- Parse best-effort `tokens used` from Ralph stderr.
- Populate enriched Arena ledger rows.
- Best-effort refresh the scorecard digest after Arena report write.

---

### Task 1: Generator emits the separate harness scorecard report

**Files:**
- Modify: `/Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs`
- Create: `/Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs`

**Interfaces:**
- Consumes: existing `model-bench.jsonl` rows, including old Arena rows whose `notes` match `conductor arena <run-id> profile=<profile> reason=<reason>`.
- Produces: `~/.harness/reports/model-scorecard/digest/report.json` and `~/.harness/reports/harness-scorecard/digest/report.json`.

- [ ] **Step 1: Write the failing generator test**

Create `/Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs`:

```js
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

const script = new URL("./gen-scorecard-digest.mjs", import.meta.url).pathname;

function writeLedger(home, rows) {
  const claude = path.join(home, ".claude");
  fs.mkdirSync(claude, { recursive: true });
  fs.writeFileSync(
    path.join(claude, "model-bench.jsonl"),
    rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
}

function readReport(home, project) {
  return JSON.parse(
    fs.readFileSync(
      path.join(home, ".harness", "reports", project, "digest", "report.json"),
      "utf8",
    ),
  );
}

function textOf(report) {
  return JSON.stringify(report);
}

test("generator writes model and harness scorecard reports from arena rows", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "scorecard-home-"));
  try {
    writeLedger(home, [
      {
        date: "2026-07-04",
        model: "openai-codex/gpt-5.5",
        harness: "pi",
        profile: "pi-gpt55",
        role: "arena-candidate",
        task: "patchstand-9io",
        judge: "",
        verify_passed: false,
        complexity: "S",
        project: "patchstand",
        notes: "conductor arena arena-20260704-225956-patchstand-9io profile=pi-gpt55 reason=worktree dirty after verify_cmd",
      },
      {
        date: "2026-07-04",
        model: "neuralwatt/kimi-k2.6",
        harness: "pi",
        profile: "pi-nw-kimi-k26",
        role: "arena-candidate",
        task: "warden-vy1",
        score_1_5: 4.4,
        blind_rank: 1,
        judge: "qwen37max,gpt55,nw-glm52",
        verify_passed: true,
        complexity: "S",
        project: "warden",
        arena_run_id: "arena-20260704-225738-warden-vy1",
        winner: true,
        applied: true,
        duration_ms: 120000,
        ralph_duration_ms: 90000,
        verify_duration_ms: 30000,
        tokens_used: 309466,
        notes: "conductor arena arena-20260704-225738-warden-vy1 profile=pi-nw-kimi-k26 reason=",
      },
      {
        date: "2026-07-03",
        model: "gpt-5.5",
        role: "implement",
        score_1_5: 5,
        verify_passed: true,
        complexity: "M",
        project: "hindsight",
        notes: "ordinary non-arena dispatch row",
      },
    ]);

    const result = spawnSync(process.execPath, [script], {
      env: { ...process.env, HOME: home },
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr || result.stdout);

    const model = readReport(home, "model-scorecard");
    assert.equal(model.schema, "harness-deck/report@1");
    assert.equal(model.id, "digest");

    const harness = readReport(home, "harness-scorecard");
    assert.equal(harness.schema, "harness-deck/report@1");
    assert.equal(harness.id, "digest");
    assert.equal(harness.project, "harness-scorecard");
    assert.equal(harness.status, "done");

    const text = textOf(harness);
    assert.match(text, /Arena Harness Scorecard/);
    assert.match(text, /pi-nw-kimi-k26/);
    assert.match(text, /arena-20260704-225956-patchstand-9io/);
    assert.match(text, /worktree dirty after verify_cmd/);
    assert.match(text, /309,466|309466/);
    assert.match(text, /cost data/i);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
```

- [ ] **Step 2: Run the generator test and verify it fails**

Run:

```bash
node --test /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs
```

Expected before implementation: FAIL because `harness-scorecard/digest/report.json` does not exist.

- [ ] **Step 3: Refactor generator helpers without changing output semantics**

Modify `/Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs`:

- Add a reusable `writeReport(project, id, report)` helper at the bottom and use it for the existing model report.
- Add pure helpers near `avgScore`:
  - `pct(part, total)` returns a rounded percentage integer, `0` when total is `0`.
  - `median(numbers)` returns `null` for an empty array, otherwise the numeric median.
  - `formatMs(ms)` returns `—`, `<1s`, `Ns`, `Nm Ns`, or `Nh Nm`.
  - `formatTokens(n)` returns `—` or `n.toLocaleString("en-US")`.
  - `arenaRunId(row)` uses `row.arena_run_id` first, then parses `row.notes` with `/conductor arena (\S+)/`.
  - `failureReason(row)` uses `row.failure_reason` first, then parses `row.notes` with `/ reason=(.*)$/`.
  - `costValue(row)` returns a finite number from numeric or string `row.cost_usd`, otherwise `null`.

Keep the existing model report path and console output working.

- [ ] **Step 4: Implement harness-scorecard aggregation and report blocks**

In the generator, after the existing model report data is built:

- Build `arenaRows = rows.filter((r) => r.role === "arena-candidate")`.
- Normalize each Arena row into a view object with: `runId`, `date`, `project`, `task`, `harness`, `profile`, `model`, `verified`, `score`, `rank`, `winner`, `applied`, `durationMs`, `tokensUsed`, `cost`, `reason`, `judges`.
- Group by run id for recent-run rows.
- Group by `harness` for overall harness ranking.
- Group by `${harness} / ${profile}` for profile ranking.
- Render HTML tables following the existing inline style pattern in the model report.
- Add a cost-data callout when no Arena row has `cost_usd`.
- Write the new manifest to `harness-scorecard/digest` using `writeReport("harness-scorecard", "digest", harnessReport)`.

Do not remove the small existing harness/profile table from `model-scorecard/digest`; it can stay as a compact model-scorecard cross-link.

- [ ] **Step 5: Run the generator test and verify it passes**

Run:

```bash
node --test /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs
```

Expected: PASS.

- [ ] **Step 6: Run the real generator and validate both reports**

Run:

```bash
node /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs
```

Expected: stdout includes both report paths or at least no error.

Run:

```bash
hdeck validate /Users/tfinklea/.harness/reports/model-scorecard/digest/report.json
```

Expected: `ok`.

Run:

```bash
hdeck validate /Users/tfinklea/.harness/reports/harness-scorecard/digest/report.json
```

Expected: `ok`.

- [ ] **Step 7: Commit Task 1 in chezmoi-config only**

Run:

```bash
git -C /Users/tfinklea/git/chezmoi-config status --short
```

Stage only:

```bash
git -C /Users/tfinklea/git/chezmoi-config add private_dot_local/lib/scorecard/gen-scorecard-digest.mjs private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs
```

Commit:

```bash
git -C /Users/tfinklea/git/chezmoi-config commit -m "feat(scorecard): add arena harness digest"
```

---

### Task 2: Conductor ledger rows carry Arena metadata

**Files:**
- Modify: `/Users/tfinklea/git/harness-conductor/src/ledger.rs`
- Modify: `/Users/tfinklea/git/harness-conductor/src/arena.rs`

**Interfaces:**
- Consumes: `ArenaDecision`, candidate run summaries, Ralph stderr logs.
- Produces: Arena candidate ledger rows with optional `arena_run_id`, `winner`, `applied`, `failure_reason`, `duration_ms`, `ralph_duration_ms`, `verify_duration_ms`, `tokens_used`, `cost_usd`.

- [ ] **Step 1: Add ledger serialization test for optional Arena metadata**

In `/Users/tfinklea/git/harness-conductor/src/ledger.rs`, add a test next to `append_writes_one_row_without_score`:

```rust
#[test]
fn append_writes_arena_metadata_when_present() {
    let temp = TempDir::new("ledger-arena");
    let path = temp.path().join("model-bench.jsonl");
    let row = LedgerRow {
        date: "2026-07-04".to_string(),
        model: "neuralwatt/kimi-k2.6".to_string(),
        harness: Some("pi".to_string()),
        profile: Some("pi-nw-kimi-k26".to_string()),
        role: "arena-candidate".to_string(),
        task: "warden-vy1".to_string(),
        score_1_5: Some(4.4),
        blind_rank: Some(1),
        judge: Some("qwen37max,gpt55,nw-glm52".to_string()),
        verify_passed: true,
        complexity: "S".to_string(),
        project: "warden".to_string(),
        bias_note: Some("arena blind panel".to_string()),
        notes: "conductor arena arena-20260704-225738-warden-vy1 profile=pi-nw-kimi-k26 reason=".to_string(),
        arena_run_id: Some("arena-20260704-225738-warden-vy1".to_string()),
        winner: Some(true),
        applied: Some(true),
        failure_reason: None,
        duration_ms: Some(120_000),
        ralph_duration_ms: Some(90_000),
        verify_duration_ms: Some(30_000),
        tokens_used: Some(309_466),
        cost_usd: None,
    };

    append(&path, &row).expect("append ledger");

    let content = std::fs::read_to_string(&path).expect("read ledger");
    let parsed: serde_json::Value = serde_json::from_str(content.trim()).expect("json row");
    assert_eq!(parsed["arena_run_id"], json!("arena-20260704-225738-warden-vy1"));
    assert_eq!(parsed["winner"], json!(true));
    assert_eq!(parsed["applied"], json!(true));
    assert_eq!(parsed["duration_ms"], json!(120_000));
    assert_eq!(parsed["ralph_duration_ms"], json!(90_000));
    assert_eq!(parsed["verify_duration_ms"], json!(30_000));
    assert_eq!(parsed["tokens_used"], json!(309_466));
    assert!(parsed.get("cost_usd").is_none());
}
```

- [ ] **Step 2: Run the ledger test and verify it fails**

Run:

```bash
cargo test append_writes_arena_metadata_when_present
```

Expected before implementation: compile failure because `LedgerRow` lacks the new fields.

- [ ] **Step 3: Add optional fields to `LedgerRow`**

In `/Users/tfinklea/git/harness-conductor/src/ledger.rs`, add these fields after `notes` or before it; use `#[serde(skip_serializing_if = "Option::is_none")]` on each optional field:

```rust
pub(crate) arena_run_id: Option<String>,
pub(crate) winner: Option<bool>,
pub(crate) applied: Option<bool>,
pub(crate) failure_reason: Option<String>,
pub(crate) duration_ms: Option<u64>,
pub(crate) ralph_duration_ms: Option<u64>,
pub(crate) verify_duration_ms: Option<u64>,
pub(crate) tokens_used: Option<u64>,
pub(crate) cost_usd: Option<String>,
```

Update existing `LedgerRow` initializers in tests and production to include `None` for the new fields unless real values are available.

- [ ] **Step 4: Add token parser tests**

In `/Users/tfinklea/git/harness-conductor/src/arena.rs` tests module, add tests for the pure parser:

```rust
#[test]
fn parse_tokens_used_from_ralph_stderr() {
    let stderr = "hook: Stop\nhook: Stop Completed\ntokens used\n309,466\n";
    assert_eq!(parse_tokens_used(stderr), Some(309_466));
}

#[test]
fn parse_tokens_used_ignores_missing_or_bad_values() {
    assert_eq!(parse_tokens_used("hook: Stop\n"), None);
    assert_eq!(parse_tokens_used("tokens used\nnot-a-number\n"), None);
}
```

- [ ] **Step 5: Run parser tests and verify they fail**

Run:

```bash
cargo test parse_tokens_used
```

Expected before implementation: compile failure because `parse_tokens_used` does not exist.

- [ ] **Step 6: Implement parser and timing fields**

In `/Users/tfinklea/git/harness-conductor/src/arena.rs`:

- Add `use std::time::Instant;` near the other imports.
- Extend `CandidateRun` with:

```rust
duration_ms: Option<u64>,
ralph_duration_ms: Option<u64>,
verify_duration_ms: Option<u64>,
tokens_used: Option<u64>,
```

- Add a small helper:

```rust
fn duration_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
```

- Add pure parser:

```rust
fn parse_tokens_used(stderr: &str) -> Option<u64> {
    let mut lines = stderr.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "tokens used" {
            for value in lines.by_ref() {
                let digits = value
                    .chars()
                    .filter(|ch| ch.is_ascii_digit())
                    .collect::<String>();
                if digits.is_empty() {
                    continue;
                }
                return digits.parse::<u64>().ok();
            }
        }
    }
    None
}
```

- In `run_one_candidate()`, capture total, Ralph, and verify timers. Read `stderr` after Ralph exits and call `parse_tokens_used()`.
- Ensure every early return populates the metadata fields gathered so far.

- [ ] **Step 7: Populate enriched ledger rows**

Change `append_ledger_rows()` signature so it receives `decision: &ArenaDecision` and `applied: bool`.

For each candidate row:

- `arena_run_id: Some(ctx.run_id.clone())`
- `winner: Some(decision.winner_profile.as_deref() == Some(candidate.summary.profile.as_str()))`
- `applied: Some(applied)`
- `failure_reason: (!candidate.summary.reason.is_empty()).then(|| candidate.summary.reason.clone())`
- timing/token fields from `CandidateRun`
- `cost_usd: None`

Update the call site in `arena::run()`.

- [ ] **Step 8: Run targeted Rust tests**

Run:

```bash
cargo test append_writes_arena_metadata_when_present parse_tokens_used
```

Expected: PASS.

Run:

```bash
cargo test ledger
```

Expected: PASS.

Run:

```bash
cargo test arena
```

Expected: PASS.

- [ ] **Step 9: Commit Task 2 in harness-conductor only**

Run:

```bash
git -C /Users/tfinklea/git/harness-conductor status --short
```

Stage only:

```bash
git -C /Users/tfinklea/git/harness-conductor add src/ledger.rs src/arena.rs
```

Commit:

```bash
git -C /Users/tfinklea/git/harness-conductor commit -m "feat(arena): record harness scorecard metadata"
```

---

### Task 3: Arena refreshes scorecard digest best-effort

**Files:**
- Modify: `/Users/tfinklea/git/harness-conductor/src/arena.rs`

**Interfaces:**
- Consumes: local HOME path and optional generator at `~/.local/lib/scorecard/gen-scorecard-digest.mjs`.
- Produces: best-effort regenerated harness-deck scorecard reports after an Arena run writes its own report.

- [ ] **Step 1: Add digest-refresh helper tests**

In `/Users/tfinklea/git/harness-conductor/src/arena.rs` tests module, add tests for path selection and missing-script behavior:

```rust
#[test]
fn scorecard_digest_script_path_points_under_home() {
    let home = PathBuf::from("/tmp/fake-home");
    assert_eq!(
        scorecard_digest_script_path(&home),
        home.join(".local").join("lib").join("scorecard").join("gen-scorecard-digest.mjs")
    );
}

#[test]
fn refresh_scorecard_digest_skips_missing_script() {
    let temp = TempDir::new("digest-missing");
    let warning = refresh_scorecard_digest(temp.path()).expect("missing script is not an error");
    assert!(warning.is_none());
}
```

- [ ] **Step 2: Run digest helper tests and verify they fail**

Run:

```bash
cargo test scorecard_digest
```

Expected before implementation: compile failure because helper functions do not exist.

- [ ] **Step 3: Implement digest refresh helpers**

In `/Users/tfinklea/git/harness-conductor/src/arena.rs`, add:

```rust
fn scorecard_digest_script_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("lib")
        .join("scorecard")
        .join("gen-scorecard-digest.mjs")
}

fn refresh_scorecard_digest(home: &Path) -> Result<Option<String>> {
    let script = scorecard_digest_script_path(home);
    if !script.exists() {
        return Ok(None);
    }
    let output = Command::new("node")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| ArenaError::new(format!("failed to run scorecard digest: {e}")))?;
    if output.status.success() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "scorecard digest exited {}: {}",
            status_summary(output.status.code()),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
```

This helper returns a warning string instead of failing for non-zero generator exit. Spawn failure may return `Err`; the call site must catch and warn instead of failing the Arena.

- [ ] **Step 4: Call refresh after the Arena report is written**

In `arena::run()`, after `deck::write_report(...)` succeeds and before cleanup, call:

```rust
match refresh_scorecard_digest(&home_dir()) {
    Ok(Some(warning)) => eprintln!("arena: {warning}"),
    Ok(None) => {}
    Err(e) => eprintln!("arena: scorecard digest skipped: {e}"),
}
```

Do not change Arena success/failure based on digest refresh.

- [ ] **Step 5: Run targeted and full Arena tests**

Run:

```bash
cargo test scorecard_digest
```

Expected: PASS.

Run:

```bash
cargo test arena
```

Expected: PASS.

- [ ] **Step 6: Commit Task 3 in harness-conductor only**

Run:

```bash
git -C /Users/tfinklea/git/harness-conductor add src/arena.rs
git -C /Users/tfinklea/git/harness-conductor commit -m "feat(arena): refresh scorecard digest after runs"
```

---

### Task 4: End-to-end verification and docs handoff

**Files:**
- Planned code/doc outputs are already covered by Tasks 1–3.
- Conditional handoff update: `/Users/tfinklea/git/harness-conductor/.docs/ai/current-state.md` only when Conductor work remains pending across sessions.
- Conditional handoff update: `/Users/tfinklea/git/chezmoi-config/.docs/ai/current-state.md` only when generator source is committed but live HOME apply remains pending.
- Conditional ADR update: respective `.docs/ai/decisions.md` only when implementation makes a new durable decision beyond the approved spec.

**Interfaces:**
- Consumes: completed Tasks 1–3.
- Produces: verified reports and clean git status aside from known pre-existing WIP.

- [ ] **Step 1: Run final generator verification**

Run:

```bash
node --test /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.test.mjs
```

Expected: PASS.

Run:

```bash
node /Users/tfinklea/git/chezmoi-config/private_dot_local/lib/scorecard/gen-scorecard-digest.mjs
```

Expected: writes model and harness scorecard reports.

- [ ] **Step 2: Validate reports**

Run:

```bash
hdeck validate /Users/tfinklea/.harness/reports/model-scorecard/digest/report.json
```

Expected: `ok`.

Run:

```bash
hdeck validate /Users/tfinklea/.harness/reports/harness-scorecard/digest/report.json
```

Expected: `ok`.

- [ ] **Step 3: Run final Conductor verification**

Run:

```bash
cargo test arena
```

Expected: PASS.

- [ ] **Step 4: Inspect generated harness report content**

Run:

```bash
node -e "const fs=require('fs');const r=JSON.parse(fs.readFileSync('/Users/tfinklea/.harness/reports/harness-scorecard/digest/report.json','utf8')); console.log(r.title); console.log(r.blocks.map(b=>b.title||b.type).join('\n'));"
```

Expected output includes:

```text
Arena Harness Scorecard — which harness wins
Overall harness ranking
Harness/profile ranking
Recent Arena runs
Per-run candidate stats
```

- [ ] **Step 5: Apply the handoff-doc routing rule**

If implementation spans sessions or leaves pending apply/work, add terse bullets to the relevant current-state file:

- `harness-conductor/.docs/ai/current-state.md` for pending Conductor implementation state.
- `chezmoi-config/.docs/ai/current-state.md` for pending source-ahead apply of `~/.local/lib/scorecard/gen-scorecard-digest.mjs`.

Do not duplicate rationale there; if a new durable decision was made beyond the approved spec, add it to the matching `decisions.md`.

- [ ] **Step 6: Final status check**

Run:

```bash
git -C /Users/tfinklea/git/harness-conductor status --short
```

Expected: no uncommitted files from this work.

Run:

```bash
git -C /Users/tfinklea/git/chezmoi-config status --short
```

Expected: only known pre-existing WIP plus committed generator changes; no unstaged files from this work.

## Self-review checklist

- Spec coverage:
  - New `harness-scorecard/digest` report: Task 1.
  - Model scorecard continuity: Task 1 keeps existing report.
  - Historical Arena row compatibility: Task 1 test + helpers.
  - Conductor metadata fields: Task 2.
  - Best-effort token parsing: Task 2.
  - Digest refresh after Arena: Task 3.
  - Validation and no-cost callout: Tasks 1 and 4.
- Placeholder scan: no unresolved-marker strings, no unspecified tests, no invented cost values.
- Type consistency:
  - Ledger fields use snake_case JSON names matching Rust field names.
  - Generator expects `arena_run_id`, `duration_ms`, `tokens_used`, `winner`, `applied`, `failure_reason`, `cost_usd`.
  - Report ids/projects match the approved spec: `model-scorecard/digest`, `harness-scorecard/digest`.
