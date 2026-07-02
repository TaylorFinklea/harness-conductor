# `agy -p` silent no-op — root cause + detection

**VERDICT:** `agy -p` against `Gemini 3.5 Flash (High)` is **effectively OFF
the conductor roster** until quota resets (~2026-07-06, ~110h from
2026-07-01 19:11 UTC). Junior-tier work falls through to seniors for the
duration. Re-verify before re-enabling — the symptom is a **silent
exit 0**, not a failure signal, so naïve success-checks will mis-report
"completed" and the conductor will move on as if everything worked.

Follow-up (separate task, **not done here**): remove/comment the
`gemini-3.5-flash` row in `conductor.toml` and add a matching entry to
the model scorecard; this doc is the evidence those edits should cite.

---

## What we observed

Two `agy -p` invocations on 2026-07-01 (19:09:57 and 19:11:14) targeting
`Gemini 3.5 Flash (High)` both:

1. Retried internally a handful of times.
2. Hit `RESOURCE_EXHAUSTED (code 429): Individual quota reached` from
   the upstream `daily-cloudcode-pa` API.
3. **Exited 0 with 0-byte stdout** — a fail-open exit-code bug.

There was no `PlannerResponse.ModifiedResponse` payload, no streamed
text, no error surfaced on stderr or in the exit code. From the caller's
perspective the process "succeeded".

`Medium`-reasoning calls in the same window **succeeded** — the quota is
per-model (and per-reasoning-tier), not account-wide.

## Evidence

| file | RESOURCE_EXHAUSTED hits | first 429 timestamp | reset countdown |
|---|---|---|---|
| `~/.gemini/antigravity-cli/log/cli-20260701_190957.log` | 6 | 19:09:58 | 110h14m43s |
| `~/.gemini/antigravity-cli/log/cli-20260701_191114.log` | 6 | 19:11:15 | (same window) |

Canonical error line (verbatim, from `cli-20260701_190957.log`):

```
RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade
your subscription to increase your limits. Resets in 110h14m43s.
```

Reset window: ~110h from 2026-07-01 19:11 → **~2026-07-06**.

Adjacent observation from `cli-20260701_191114.log`:

```
Creating CLI server backend: product=antigravity
  workspaceDirs=[/Users/tfinklea/git/harness-conductor
                  /Users/tfinklea/git/harness-conductor]
```

`agy` appears to register `cwd` once via project-scope and once again via
`--add-dir "$PWD"`, producing a duplicated entry. Harmless to the user
(no observed functional impact), but a footgun if any future
per-workspace logic assumes uniqueness. Note it; do not act on it.

## The detection rule (the only reliable signal)

**Never trust `agy -p`'s exit code or stdout length.** The
`RESOURCE_EXHAUSTED` failure path returns `exit 0` with empty stdout.

The only reliable success/failure signal is the per-invocation log
under `~/.gemini/antigravity-cli/log/`. Conductor's worker wrapper
should:

1. Capture the wall-clock start time (`date +%Y%m%d_%H%M%S`).
2. After the `agy` process exits, grep the most recent
   `cli-<startTime>*.log` (or any `cli-<same-day>*.log` created during
   the run) for `RESOURCE_EXHAUSTED`.
3. Treat a hit as a hard failure, regardless of exit code.

Concretely:

```bash
# fail detector (any non-zero exit = fail)
LOG=~/.gemini/antigravity-cli/log/cli-$(date +%Y%m%d)*.log
if grep -q RESOURCE_EXHAUSTED $LOG 2>/dev/null; then
  echo "agy: quota exhausted" >&2
  exit 1
fi
```

A more robust wrapper would: (a) snapshot `ls -t` of the log dir
*before* the invocation, (b) run `agy`, (c) diff against the snapshot
to find the new file, (d) grep that file. The simple per-day glob above
is fine for the common case; the snapshot-diff variant handles the
midnight-rollover edge and concurrent invocations.

Per-model accounting: scope the grep — or, if a single invocation
touches only one model, trust the run-scoped grep. Don't generalize
"RESOURCE_EXHAUSTED seen anywhere today = dead backend" — quotas are
per-model, so a Medium call and a High call from the same account can
have opposite outcomes on the same day.

## Working-invocation guidance (UNVERIFIED)

If/when `agy -p` is needed against a non-quota-exhausted model, the
safer wrapper is `--sandbox` + an explicit strict policy, **not**
`--dangerously-skip-permissions`. The current conductor invocation
template uses `--dangerously-skip-permissions` because the model is
junior-tier and trusted to stay in lane, but a quota-exhaustion retry
loop with a hostile or hallucinated directive is exactly the situation
where "skip permissions" amplifies the blast radius. Status:
**UNVERIFIED** — I did not build or test the `--sandbox` wrapper;
recommend it as the next concrete artifact to produce, alongside
re-introducing `gemini-3.5-flash` to the roster.

## What this means for the roster

`conductor.toml` currently lists:

```toml
[[roster]]
name = "gemini-3.5-flash"
tier = "junior"
backend = "agy"
dispatch_id = "Gemini 3.5 Flash (High)"
ceiling = "S"
efficiency = "lean"
```

…which is the **only** junior-tier row. With `gemini-3.5-flash` out,
the roster has no `junior` tier at all, so per the tiered-routing rules
junior items either:

- escalate to `senior` (closest match — `gpt-5.5`, `minimax-m3`,
  `qwen3.7-max`, `glm-5.2`), or
- flag as `tier_floor: lead` and queue for human triage.

Routing choice is a follow-up, not this doc's call. The **immediate**
operational consequence is: do not expect junior-tier work to be
absorbed for the next ~4 days.

## Follow-ups (separate tasks, not this bead)

1. `conductor.toml` — comment or remove the `gemini-3.5-flash` row
   with a "quota exhausted, re-enable after ~2026-07-06" pointer back
   to this doc. Add a follow-up bead if calendar-reminder style is
   preferred over a stale entry.
2. `~/.claude/model-scorecard.md` — log the `Gemini 3.5 Flash (High)`
   silent-no-op incident against the `agy` model row; this is the
   paper trail `conductor roster drift` checks against.
3. Build a `--sandbox` agy wrapper with a strict policy (see
   UNVERIFIED above) before re-enabling.

## Re-enable checklist

Before flipping `gemini-3.5-flash` back on:

- [ ] Date is past ~2026-07-06 (or whatever the current API says for
      "Resets in …" — **re-check, do not trust this doc's calendar**).
- [ ] `--sandbox` wrapper + strict policy exists and is exercised by a
      smoke test.
- [ ] Detection rule from this doc is wired into the conductor's
      agy-worker path (grep-for-`RESOURCE_EXHAUSTED` post-run).
- [ ] A scorecard entry exists for the silent-no-op failure mode so
      `conductor roster drift` doesn't quietly pass.
