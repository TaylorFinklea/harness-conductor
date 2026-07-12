# OpenWiki pilot — spec

**Status**: ready — bead filed (2026-07-07)
**Owning repo**: `~/git/conductor` (pilot target; portable to `~/git/harness-deck`)
**Tracking bead**: `conductor-1qh`
**tier_floor**: `senior` · **complexity**: `M`

## Goal

Evaluate whether LangChain's [OpenWiki](https://github.com/langchain-ai/openwiki)
([announcement](https://www.langchain.com/blog/introducing-openwiki-an-open-source-agent-for-repo-documentation))
— a CLI that auto-generates/maintains agent-facing reference docs in `openwiki/` from git diffs —
adds value over our hand-authored `.docs/ai/`.

Evaluation pilot only: mechanical setup + one doc-generation pass + a drafted verdict. The
**adopt/reject call is the user's (Lead)**, not the executing agent's.

Non-goal for this pass: no scheduled Action, no chezmoi-config changes. Those are gated follow-ons.

## Layer boundary (why this isn't redundant with `.docs/ai/`)

- OpenWiki output = **reference** (what the code *is*: architecture/overview).
- `.docs/ai/` = **state + decisions** (what's in flight, why). `decisions.md` stays authoritative.
- The pilot must confirm the two layers coexist without an agent treating a stale auto-wiki as
  authoritative over `decisions.md`.

## Verified facts (OpenWiki + target repo)

OpenWiki (from repo README / `openwiki/quickstart.md` / `openwiki/cli/usage.md`, confirm at runtime with `openwiki --help`):

- Install: `npm install -g openwiki`.
- Flags: `--init [msg]`, `--update [msg]`, `-p`/`--print` (non-interactive), `--modelId <id>` / `--model-id <id>`, `--dry-run`, `-h`.
- Creds work **env-var-only** under `--print`: "If stdin is not a TTY (e.g. CI), or `--print` is used, the CLI requires a provider API key ... in `~/.openwiki/.env` or present in the environment." → we pass keys via env and never write the dotfile.
- Provider key env vars: `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `BASETEN_API_KEY`, `FIREWORKS_API_KEY`, `OPENAI_COMPATIBLE_API_KEY` (+ `OPENAI_COMPATIBLE_BASE_URL`).
- Model via `OPENWIKI_MODEL_ID` env or `--modelId`.
- Writes: `openwiki/**` (committed), `openwiki/.last-update.json`, `~/.openwiki/.env` (creds — avoid), and may inject pointers into `AGENTS.md`/`CLAUDE.md`.
- Ships an example Action that runs `openwiki --update --print` daily and **opens a PR** (branch `openwiki/update`), not a direct commit.

Target repo (`conductor`):

- Rust; backlog in **beads** (`bd` 1.0.5); `.docs/ai/` with `decisions.md` + `phases/`.
- **No root `AGENTS.md`/`CLAUDE.md`** → OpenWiki's injection has nothing curated to clobber (de-risked). If it *creates* those files, review before keeping.
- `.gitignore` has no `openwiki`/`.env` entry. Generated `openwiki/` is meant to be committed; `~/.openwiki/.env` lives in `$HOME`, outside the repo.

## Provider / key wiring (keychain-safe — the crux)

Keys stay in Keychain (see `~/AGENTS.md` § API Keys); export into the env for the single command only. **Do not create `~/.openwiki/.env` with a real key.**

- **Preferred (cheap GLM, flat-rate):** OpenAI-compatible provider →
  - `OPENAI_COMPATIBLE_API_KEY=$OLLAMA_API_KEY` (keychain + launchd loaded)
  - `OPENAI_COMPATIBLE_BASE_URL=` Ollama-Cloud OpenAI-compatible endpoint — **read from existing config, don't guess:** `rg -n 'ollama-cloud|ollama\.com' ~/git/chezmoi-config`
  - `OPENWIKI_MODEL_ID=glm-5.2`
- **Zero-config fallback:** plain OpenAI provider with already-exported `OPENAI_API_KEY` (pricier; a one-shot init on one repo is pennies). Use if the OpenAI-compatible base-URL wiring fights you — pilot goal is doc quality, not perfect routing.

## Steps

1. `npm install -g openwiki`.
2. From `~/git/conductor`, export the chosen key into the env (Preferred or Fallback). Do not let onboarding persist a key.
3. `openwiki --init --print` (`--print` = non-interactive; avoids writing `~/.openwiki/.env`).
4. Inspect generated `openwiki/` against actual `src/` — accurate, or hallucinated architecture?
5. Review any diff OpenWiki makes to `AGENTS.md`/`CLAUDE.md`. Keep pointers minimal and non-conflicting with `.docs/ai/`; revert intrusive edits.
6. Secret scan before commit: `git grep -nIE '(sk-[A-Za-z0-9]{20,}|_API_KEY=|BEGIN [A-Z ]*PRIVATE KEY)' -- openwiki/ || echo clean`.
7. Draft ADR in `.docs/ai/decisions.md`: what OpenWiki produced, comparison to `.docs/ai/` (reference-vs-state), cost/run, recommendation (adopt / adopt-with-guardrails / reject). **Flag the final call for the user.**
8. One commit: generated `openwiki/` + the ADR.

## Files

- **New (committed):** `openwiki/**`.
- **Modified:** `.docs/ai/decisions.md` (ADR).
- **Possibly created by OpenWiki:** `AGENTS.md` / `CLAUDE.md` — review before keeping.
- **Must NOT appear:** `~/.openwiki/.env` holding a real key; any key value inside the repo.

## Acceptance

- OpenWiki ran once; `openwiki/` docs exist and are committed.
- No plaintext key persisted (`~/.openwiki/.env` absent/keyless); secret scan clean.
- `AGENTS.md`/`CLAUDE.md` injection reviewed; `.docs/ai/` still authoritative for state/decisions.
- ADR written with an explicit recommendation; **adopt/reject escalated to the user** (agent does not self-authorize adoption).

## Verify

Mechanical (`verify_cmd` metadata):

```
test -d openwiki && ! test -s "$HOME/.openwiki/.env" && ! git grep -qIE '(sk-[A-Za-z0-9]{20,}|_API_KEY=)' -- openwiki
```

Substantive gate = **named human check**: user reviews doc quality + the ADR recommendation.

## Guardrails

- No scheduled Action this pass; no chezmoi-config changes.
- Never `git add -f` or commit a key; never write keys to source or `~/.openwiki/.env`.
- If rejected: `rm -rf openwiki/`, `npm rm -g openwiki`, ADR records why.

## Gated follow-on (file only if verdict = adopt)

Separate bead(s):

1. Daily `workflow_dispatch`+cron Action using OpenWiki's PR-only example (branch `openwiki/update`), GLM/OpenAI-compatible key via CI secret. Keep PR-only so injected instruction-file edits stay reviewable.
2. chezmoi-config item to template the CLI install into dotfiles.

## Filing

```bash
bd create "Pilot OpenWiki on conductor (eval, keychain-safe)" -t task -p 2 \
  -d "Evaluation pilot per .docs/ai/phases/openwiki-pilot-spec.md: install OpenWiki, one --init pass with a keychain-sourced key (no ~/.openwiki/.env), commit generated openwiki/ docs, draft adopt/reject ADR in .docs/ai/decisions.md. Adopt call is the user's."
# then, using the returned id:
bd update <id> \
  --set-metadata=tier_floor=senior \
  --set-metadata=complexity=M \
  --set-metadata=verify_cmd='test -d openwiki && ! test -s "$HOME/.openwiki/.env" && ! git grep -qIE "(sk-[A-Za-z0-9]{20,}|_API_KEY=)" -- openwiki'
```
