# `bd --readonly` enforcement investigation

**VERDICT: enforced** — every write verb tested is rejected with exit code 1 and a
named-operation error, and the issue is observably unchanged after every attempt.
Reads work normally.

**Recommendation:** conductor's workers can rely on `--readonly` as a write
sandbox today (2026-07-01, bd 1.0.5 / Homebrew). Wrap the worker's bd invocation
in `--readonly` for any context where a worker is handed bd access and we don't
want it mutating the queue.

---

## Setup (all in throwaway scratch, never in `~/git`)

```bash
SCRATCH=$(mktemp -d -t bd-readonly-XXXXXX)
cd "$SCRATCH"
bd init --non-interactive -p scratch < /dev/null   # exit 0
bd create "dummy" -t task < /dev/null             # exit 0 → scratch-isl
```

Environment:
- `bd version 1.0.5 (Homebrew)`
- Flag is documented in `bd --help`: `--readonly  Read-only mode: block write
  operations (for worker sandboxes)` (sibling: `--sandbox`, which is for Dolt
  auto-push — not the same thing).

## Reads under `--readonly` (must still work)

| command | exit | result |
|---|---|---|
| `bd --readonly ready` | 0 | `○ scratch-isl ● P2 dummy` / "Ready: 1 issues with no active blockers" |
| `bd --readonly show scratch-isl` | 0 | full issue dump: `○ scratch-isl · dummy [● P2 · OPEN]` |

## Mutations under `--readonly` (must be blocked)

| command | exit | stdout/stderr | state changed? |
|---|---|---|---|
| `bd --readonly create "x" -t task` | **1** | `Error: operation 'create' is not allowed in read-only mode` | no |
| `bd --readonly update scratch-isl --status in_progress` | **1** | `Error: operation 'update' is not allowed in read-only mode` | no |
| `bd --readonly close scratch-isl --reason test` | **1** | `Error: operation 'close' is not allowed in read-only mode` | no |
| `bd --readonly comment scratch-isl "test"` | **1** | `Error: operation 'comment' is not allowed in read-only mode` | no |
| `bd --readonly ready --claim` | **1** | `Error: operation 'ready --claim' is not allowed in read-only mode` | no |

Post-test state check (no `--readonly`):
- `bd show scratch-isl` → still `○ scratch-isl · dummy [● P2 · OPEN]`, Updated:
  2026-07-02 (unchanged)
- `bd count` → still `1`

## Notes

- Error format is consistent: `Error: operation '<verb>' is not allowed in
  read-only mode` — easy for an orchestrator to grep/parse. The verb is the
  exact subcommand (or `ready --claim` for that combo), so logs are grep-friendly.
- The flag appears at the global position (`bd --readonly <subcmd> ...`).
  The spec already mandates `bd -C <repo> <cmd>` to avoid `cd`; combining gives
  `bd -C <repo> --readonly <cmd> < /dev/null` — no `cd`, no stdin hang,
  no writes.
- `bd ready --claim` is a mutation, and the `--readonly` guard correctly
  rejects the *whole* invocation rather than the `--claim` half — important
  per the spec's "never call it" line, because a worker that *just* ran
  `bd --readonly ready` and got the same answer wouldn't accidentally claim.
- Scope was the five mutations the spec called out; I did not expand to
  `label`/`assign`/`delete`/`sync`/etc. — out of scope. If a future task wants
  a full enumeration of every write verb, that's a separate sweep.
- All experiments happened in a throwaway `mktemp -d` temp dir that is now
  deleted. `~/git` was never touched.
