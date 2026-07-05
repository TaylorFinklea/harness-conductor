# current-state.md — harness-conductor

Branch: main
Bead in flight: `conductor-b9h` (Arena: worktree-dirty post-verify_cmd yields false-negative eligibility)

## Plan

- [ ] Fix `conductor-b9h`: Arena marks verify-passing candidates as "worktree dirty after verify_cmd" because ralph leaves untracked arena-scaffolding (`.docs/ai/loop-prompt.md`) in the worktree after the candidate commits + verify passes. Make Conductor's eligibility check distinguish "untracked scaffolding" from "uncommitted source changes" — either (a) teach ralph's arena run to clean up / gitignore the per-run scaffolding before the worktree-dirty check, or (b) teach Conductor's eligibility check to honour a per-repo scaffolding-ignore list so untracked arena scaffolding doesn't fail eligibility. Touches `src/arena.rs` (around line 827 where `worktree dirty after verify_cmd` is set, and line 857 where `loop-prompt.md` is written). Read the bead for full context: `bd show conductor-b9h`. Repro command in the bead. Do NOT make the arena eligibility check silently accept untracked files in general — only the documented scaffolding paths.
  - `Verify: cargo test arena`
