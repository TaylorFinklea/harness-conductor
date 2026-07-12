# current-state.md — conductor

Branch: main
`cargo test` 237/238 — 1 pre-existing env failure (`harness_deck_validate`; `harness-deck` not on PATH), unrelated to rebrand.
Rebranded `harness-conductor` → `conductor`: repo refs + chezmoi-personal source (`f95115b`) + GitHub repo renamed, `origin` updated.

## Plan

## Blockers
- Rebrand cutover unfinished — 3 human steps in roadmap Now (rename `backlog-harness-conductor`; `mv` the dir; `chezmoi apply`). Steps 2 and 3 must not be separated: the move deletes the path live skills/ralph still reference.

## Open questions
