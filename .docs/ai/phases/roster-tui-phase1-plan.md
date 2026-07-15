# Roster TUI — Phase 1 Implementation Plan (schema + dispatcher)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Conductor honor `enabled` on roster rows and a new `[[provider]]` table, so a model or an entire provider can be taken out of rotation without deleting config.

**Architecture:** `conductor.toml` gains a first-class `[[provider]]` table and an optional `enabled` key on `[[roster]]` rows. At parse time, each `RosterEntry` gets a resolved `effective_enabled = enabled && provider.enabled`, so the dispatcher filters on one precomputed field instead of threading a provider list through every call site. Selection and the fallback walk share the rule: a disabled model is never chosen and never run.

**Tech Stack:** Rust 2024, no new dependencies (serde / serde_json / chrono only). Hand-rolled TOML parser in `src/config.rs`.

**Spec:** `.docs/ai/phases/roster-tui-spec.md` (revised after adversarial review). Phases 2 (line-span editor) and 3 (ratatui TUI) get their own plans.

## Global Constraints

- **No new dependencies.** Phase 1 adds none.
- `unsafe_code = "forbid"`, `missing_docs = "warn"`, clippy `all` + `pedantic` at warn.
- **NEVER run bare `cargo fmt`** — this repo's baseline is not rustfmt-clean and it churns unrelated files. Scope it: `cargo fmt -- src/config.rs`.
- `conductor.toml` is the live config the fleet dispatches from. It must parse after **every** commit in this plan.
- Full test suite: `cargo test`. Baseline is 237/238 — `harness_deck_validate` fails when `harness-deck` isn't on PATH. That one pre-existing failure is expected; **any other** failure is yours.

## Sequencing note (supersedes the spec)

The spec called Phase 1 an atomic mega-commit, because provider-resolution validation would reject a config with zero `[[provider]]` blocks. **Seeding the blocks before adding the validation dissolves that problem.** Tasks 1→2→3 each leave `conductor.toml` parsing. Do not reorder them.

---

### Task 1: `[[provider]]` table — struct, parsing, and the seed

**Files:**
- Modify: `src/config.rs` (add `Provider`, `parse_providers`; extend `Config` and the `from_doc` allowlist)
- Modify: `conductor.toml` (seed 7 provider blocks)

**Interfaces:**
- Produces: `pub(crate) struct Provider { pub(crate) name: String, pub(crate) enabled: bool }`; `Config.providers: Vec<Provider>`. Task 3 consumes both.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/config.rs`:

```rust
#[test]
fn providers_parse_with_enabled_defaulting_true() {
    let src = "[[provider]]\nname = \"anthropic\"\n\n[[provider]]\nname = \"neuralwatt\"\nenabled = false\n";
    let cfg = parse_str(src).expect("provider config");
    assert_eq!(cfg.providers.len(), 2);
    assert_eq!(cfg.providers[0].name, "anthropic");
    assert!(cfg.providers[0].enabled, "enabled must default to true");
    assert_eq!(cfg.providers[1].name, "neuralwatt");
    assert!(!cfg.providers[1].enabled);
}

#[test]
fn duplicate_provider_name_is_rejected() {
    let src = "[[provider]]\nname = \"anthropic\"\n\n[[provider]]\nname = \"anthropic\"\n";
    let err = parse_str(src).expect_err("duplicate provider must fail");
    assert!(
        err.message.contains("duplicate provider name"),
        "unexpected: {}",
        err.message
    );
}

#[test]
fn unknown_provider_key_is_rejected() {
    let src = "[[provider]]\nname = \"anthropic\"\nbogus = \"x\"\n";
    let err = parse_str(src).expect_err("unknown provider key must fail");
    assert!(
        err.message.contains("unknown provider key"),
        "unexpected: {}",
        err.message
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::providers_parse_with_enabled_defaulting_true config::tests::duplicate_provider_name_is_rejected config::tests::unknown_provider_key_is_rejected`

Expected: FAIL — `no field 'providers' on type 'Config'`, and `parse_str` returns `unknown config key: provider`.

- [ ] **Step 3: Add the `Provider` struct**

In `src/config.rs`, immediately after the `RosterEntry` struct (which ends at line 297):

```rust
/// A provider/account lane (`[[provider]]`). Roster rows name one via their
/// `provider` field; disabling the provider darkens every model on it.
#[derive(Debug, Clone)]
pub(crate) struct Provider {
    pub(crate) name: String,
    /// Optional in TOML; defaults to `true`.
    pub(crate) enabled: bool,
}
```

- [ ] **Step 4: Add `providers` to `Config`**

In the `Config` struct (`src/config.rs:439-453`), add after `roster`:

```rust
    /// Provider lanes (`[[provider]]`). A roster row's `provider` must resolve
    /// to one of these (enforced from Task 3 onward).
    pub(crate) providers: Vec<Provider>,
```

- [ ] **Step 5: Write `parse_providers`**

Add next to `parse_roster` in `src/config.rs`:

```rust
fn parse_providers(node: Option<&Node>) -> Result<Vec<Provider>> {
    let entries = match node {
        None => return Ok(Vec::new()),
        Some(Node::Tables(v)) => v,
        Some(_) => {
            return Err(ConfigError::new(
                "provider must be an array of tables ([[provider]])",
            ));
        }
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for (i, t) in entries.iter().enumerate() {
        for key in t.keys() {
            if !matches!(key.as_str(), "name" | "enabled") {
                return Err(ConfigError::new(format!(
                    "unknown provider key in entry {i}: {key}"
                )));
            }
        }
        let name = get_required_str_at("provider", t, i, "name")?;
        let enabled = match t.get("enabled") {
            Some(node) => expect_bool("provider.enabled", node)?,
            None => true,
        };
        if !seen.insert(name.clone()) {
            return Err(ConfigError::new(format!("duplicate provider name: {name}")));
        }
        out.push(Provider { name, enabled });
    }
    Ok(out)
}
```

- [ ] **Step 6: Wire it into `from_doc`**

In `from_doc` (`src/config.rs:875`), add `| "provider"` to the top-level key allowlist, after `"roster"`:

```rust
                | "roster"
                | "provider"
                | "repo_policy"
```

Then, after `let roster = parse_roster(doc.get("roster"))?;`, add:

```rust
    let providers = parse_providers(doc.get("provider"))?;
```

And add `providers,` to the returned `Config { … }` literal, after `roster,`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib config::tests::providers_parse_with_enabled_defaulting_true config::tests::duplicate_provider_name_is_rejected config::tests::unknown_provider_key_is_rejected`

Expected: PASS (3 passed).

- [ ] **Step 8: Seed the 7 provider blocks in `conductor.toml`**

Insert immediately **before** the first `[[roster]]` block (line 283). These are the 7 providers already named by existing roster rows — verify with `grep '^provider = ' conductor.toml | sort -u`:

```toml
# Provider lanes. A roster row's `provider` must name one of these.
# `enabled = false` darkens every model on that provider.
[[provider]]
name = "anthropic"

[[provider]]
name = "opencode-go"

[[provider]]
name = "ollama-cloud"

[[provider]]
name = "neuralwatt"

[[provider]]
name = "openai-codex"

[[provider]]
name = "google-ai-studio"

[[provider]]
name = "agy"
```

- [ ] **Step 9: Verify the live config still parses**

Run: `cargo test --lib config::tests::checked_in_config_parses_and_has_phase2_roster_entries`

Expected: PASS.

Run: `cargo run -- config check --config conductor.toml`

Expected: `config: valid (24 roster entries)`.

- [ ] **Step 10: Commit**

```bash
cargo fmt -- src/config.rs
git add src/config.rs conductor.toml
git commit -m "feat(config): add [[provider]] table and seed the 7 live lanes"
```

---

### Task 2: `enabled` on roster rows

**Files:**
- Modify: `src/config.rs` (`RosterEntry`, `parse_roster` allowlist + construction)
- Modify: `src/roster_drift.rs:532`, `src/verify.rs:1380`, `src/triage.rs:496` (test helpers that build `RosterEntry` literals)

**Interfaces:**
- Produces: `RosterEntry.enabled: bool` (raw, from TOML, default `true`) and `RosterEntry.effective_enabled: bool` (provisionally `= enabled`; Task 3 resolves it against the provider). Tasks 3–5 consume `effective_enabled`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/config.rs`:

```rust
#[test]
fn roster_enabled_defaults_true_and_parses_false() {
    let src = "[[roster]]\nname = \"a\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\n\n[[roster]]\nname = \"b\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\nenabled = false\n";
    let cfg = parse_str(src).expect("roster config");
    assert!(cfg.roster[0].enabled, "enabled must default to true");
    assert!(!cfg.roster[1].enabled);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib config::tests::roster_enabled_defaults_true_and_parses_false`

Expected: FAIL — `unknown roster key in entry 1: enabled`.

- [ ] **Step 3: Add the fields to `RosterEntry`**

In `src/config.rs`, add to the `RosterEntry` struct (after `fallback`, before the closing brace at line 297):

```rust
    /// Raw `enabled` from TOML; optional, defaults to `true`.
    pub(crate) enabled: bool,
    /// `enabled && provider.enabled`, resolved at parse time (Task 3). This is
    /// the field the dispatcher filters on. The TUI reads `enabled` and the
    /// provider separately to show *why* a model is dark.
    pub(crate) effective_enabled: bool,
```

- [ ] **Step 4: Parse it in `parse_roster`**

Add `| "enabled"` to the roster key allowlist (`src/config.rs:1197-1202`, the `matches!` ending in `"reasoning_effort"`).

Then, just before the `if !seen.insert(name.clone())` duplicate check (`src/config.rs:1244`), add:

```rust
        let enabled = match t.get("enabled") {
            Some(node) => expect_bool("roster.enabled", node)?,
            None => true,
        };
```

And add both fields to the `out.push(RosterEntry { … })` literal, after `fallback,`:

```rust
            enabled,
            effective_enabled: enabled,
```

- [ ] **Step 5: Fix the three test-helper struct literals**

Each fails to compile with `missing fields 'enabled' and 'effective_enabled'`. In **each** of `src/roster_drift.rs:532` (`cfg_entry`), `src/verify.rs:1380`, and `src/triage.rs:496` (`roster_entry`), add these two lines to the `RosterEntry { … }` literal:

```rust
        enabled: true,
        effective_enabled: true,
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`

Expected: PASS — same 237/238 as baseline (`harness_deck_validate` still fails on missing PATH binary; nothing else).

- [ ] **Step 7: Commit**

```bash
cargo fmt -- src/config.rs
git add src/config.rs src/roster_drift.rs src/verify.rs src/triage.rs
git commit -m "feat(config): add enabled to roster rows (defaults true)"
```

---

### Task 3: Provider resolution + `effective_enabled`

This is where the gate becomes real. It is safe **only because Task 1 already seeded the provider blocks.**

**Files:**
- Modify: `src/config.rs` (add `resolve_provider_gate`, call it from `from_doc`)

**Interfaces:**
- Consumes: `Config.providers` (Task 1), `RosterEntry.enabled` (Task 2).
- Produces: a fully-resolved `RosterEntry.effective_enabled`. Tasks 4–5 filter on it.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/config.rs`:

```rust
#[test]
fn undeclared_provider_is_rejected() {
    let src = "[[roster]]\nname = \"a\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\nprovider = \"typo-lane\"\n";
    let err = parse_str(src).expect_err("undeclared provider must fail closed");
    assert!(
        err.message.contains("undeclared provider"),
        "unexpected: {}",
        err.message
    );
}

#[test]
fn empty_provider_bypasses_the_gate() {
    // Legacy/test shape: `provider` omitted -> "" -> no provider gate.
    let src = "[[roster]]\nname = \"a\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\n";
    let cfg = parse_str(src).expect("empty provider parses");
    assert!(cfg.roster[0].effective_enabled);
}

#[test]
fn disabled_provider_darkens_its_models() {
    let src = "[[provider]]\nname = \"nw\"\nenabled = false\n\n[[roster]]\nname = \"on-nw\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\nprovider = \"nw\"\n";
    let cfg = parse_str(src).expect("config");
    assert!(cfg.roster[0].enabled, "the row itself is still enabled");
    assert!(
        !cfg.roster[0].effective_enabled,
        "but the provider is off, so it is effectively disabled"
    );
}

#[test]
fn disabled_row_on_enabled_provider_is_effectively_disabled() {
    let src = "[[provider]]\nname = \"p\"\n\n[[roster]]\nname = \"a\"\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\nprovider = \"p\"\nenabled = false\n";
    let cfg = parse_str(src).expect("config");
    assert!(!cfg.roster[0].effective_enabled);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::undeclared_provider_is_rejected config::tests::disabled_provider_darkens_its_models`

Expected: FAIL — `undeclared_provider_is_rejected` gets `Ok` instead of `Err`; `disabled_provider_darkens_its_models` asserts `!effective_enabled` but gets `true` (Task 2 left it provisional).

- [ ] **Step 3: Write `resolve_provider_gate`**

Add next to `parse_providers` in `src/config.rs`:

```rust
/// Resolves each roster row's `effective_enabled` against its provider.
///
/// An empty `provider` (the default when the key is omitted) bypasses the gate
/// — a legacy/test-only shape. A non-empty `provider` MUST resolve to a
/// declared `[[provider]]` block; a typo fails closed here rather than
/// silently darkening a model at dispatch.
fn resolve_provider_gate(roster: &mut [RosterEntry], providers: &[Provider]) -> Result<()> {
    for entry in roster.iter_mut() {
        if entry.provider.is_empty() {
            entry.effective_enabled = entry.enabled;
            continue;
        }
        let Some(provider) = providers.iter().find(|p| p.name == entry.provider) else {
            return Err(ConfigError::new(format!(
                "roster entry {:?} names undeclared provider {:?} (add a [[provider]] block)",
                entry.name, entry.provider
            )));
        };
        entry.effective_enabled = entry.enabled && provider.enabled;
    }
    Ok(())
}
```

- [ ] **Step 4: Call it from `from_doc`**

In `from_doc`, change the roster binding to be mutable and resolve the gate after providers are parsed. Replace:

```rust
    let roster = parse_roster(doc.get("roster"))?;
    let providers = parse_providers(doc.get("provider"))?;
```

with:

```rust
    let mut roster = parse_roster(doc.get("roster"))?;
    let providers = parse_providers(doc.get("provider"))?;
    resolve_provider_gate(&mut roster, &providers)?;
```

(`repo_policies` is parsed after this and borrows `&roster` immutably — that still compiles, because the mutable borrow ends here.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib config::tests`

Expected: PASS. The four new tests pass, and the pre-existing expect-OK fixtures still parse — they omit `provider`, so they take the empty-provider bypass.

- [ ] **Step 6: Verify the live config still parses**

Run: `cargo run -- config check --config conductor.toml`

Expected: `config: valid (24 roster entries)`. All 24 rows resolve against the Task 1 seed.

- [ ] **Step 7: Commit**

```bash
cargo fmt -- src/config.rs
git add src/config.rs
git commit -m "feat(config): resolve effective_enabled; undeclared provider fails closed"
```

---

### Task 4: Selection excludes disabled models — and says so

The review's M3: if `enabled` goes into `candidate_rejection`, an all-disabled tier returns `None` from `select_candidate` and gets flagged **`OverCeiling`** — reporting "you turned these off" as "this item is too hard." A distinct outcome is required.

**Files:**
- Modify: `src/triage.rs` (add `Selection` enum + `Flag::AllDisabled`; filter in `select_candidate`; match in `route`)

**Interfaces:**
- Consumes: `RosterEntry.effective_enabled` (Task 3).
- Produces: `enum Selection<'a> { Chosen(&'a RosterEntry), OverCeiling, AllDisabled }`; `Flag::AllDisabled { repo, issue_id, tier }`.
- **`candidate_rejection` is NOT modified.** Task 5 depends on it keeping its current three variants' semantics.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/triage.rs`. Use the existing `roster_entry` helper (`src/triage.rs:496`) — read it first to match its parameter order.

```rust
#[test]
fn disabled_model_is_never_selected() {
    let mut only = roster_entry("only", Tier::Junior, Ceiling::S, Efficiency::Lean);
    only.effective_enabled = false;
    let mut backup = roster_entry("backup", Tier::Senior, Ceiling::M, Efficiency::Std);
    backup.effective_enabled = true;
    let roster = vec![only, backup];
    let routing = RoutingFields {
        tier_floor: Tier::Junior,
        complexity: Ceiling::S,
        ..Default::default()
    };
    let got = select_candidate(
        &roster,
        &routing,
        "repo",
        CostPolicy::default(),
        &HashMap::new(),
    );
    // The junior model is off, so selection falls through to the senior one
    // rather than picking a model that would never run.
    assert!(matches!(got, Selection::Chosen(r) if r.name == "backup"));
}

#[test]
fn all_disabled_reports_all_disabled_not_over_ceiling() {
    let mut only = roster_entry("only", Tier::Junior, Ceiling::S, Efficiency::Lean);
    only.effective_enabled = false;
    let roster = vec![only];
    let routing = RoutingFields {
        tier_floor: Tier::Junior,
        complexity: Ceiling::S,
        ..Default::default()
    };
    let got = select_candidate(
        &roster,
        &routing,
        "repo",
        CostPolicy::default(),
        &HashMap::new(),
    );
    assert!(
        matches!(got, Selection::AllDisabled),
        "a qualifying-but-dark tier must NOT be reported as over-ceiling"
    );
}

#[test]
fn genuinely_over_ceiling_still_reports_over_ceiling() {
    let roster = vec![roster_entry("j", Tier::Junior, Ceiling::S, Efficiency::Lean)];
    let routing = RoutingFields {
        tier_floor: Tier::Junior,
        complexity: Ceiling::XL,
        ..Default::default()
    };
    let got = select_candidate(
        &roster,
        &routing,
        "repo",
        CostPolicy::default(),
        &HashMap::new(),
    );
    assert!(matches!(got, Selection::OverCeiling));
}
```

If `RoutingFields` has no `Default`, construct it explicitly — read its definition and fill every field; do not add a `Default` impl just for the test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib triage::tests::disabled_model_is_never_selected triage::tests::all_disabled_reports_all_disabled_not_over_ceiling`

Expected: FAIL — `cannot find type 'Selection'`.

- [ ] **Step 3: Add the `Selection` enum**

In `src/triage.rs`, immediately before `select_candidate`:

```rust
/// Outcome of routing one item to a roster entry.
#[derive(Debug)]
pub(crate) enum Selection<'a> {
    /// A model qualifies and is effectively enabled.
    Chosen(&'a RosterEntry),
    /// No roster entry qualifies on tier/ceiling/cost.
    OverCeiling,
    /// Entries qualify, but every one of them is effectively disabled. This is
    /// an operator action (a toggled-off model or provider), NOT an
    /// over-ceiling item — conflating them hides why the item didn't dispatch.
    AllDisabled,
}
```

- [ ] **Step 4: Add the `AllDisabled` flag**

In the `Flag` enum (`src/triage.rs:84-103`), after the `OverCeiling` variant:

```rust
    /// Candidates qualify, but all are effectively disabled (`enabled = false`
    /// on the row, or on its provider).
    AllDisabled {
        repo: String,
        issue_id: String,
        tier: Tier,
    },
```

- [ ] **Step 5: Filter in `select_candidate`**

Change the return type from `Option<&'a RosterEntry>` to `Selection<'a>`. The disabled check goes **after** `candidate_rejection`, so "qualifies but dark" is distinguishable from "doesn't qualify". Replace the head of the function body (`src/triage.rs:230-238`):

```rust
    let _ = repo;
    let qualifying_any: Vec<(usize, &RosterEntry)> = roster
        .iter()
        .enumerate()
        .filter(|(_, r)| candidate_rejection(r, routing, repo_cost_policy).is_none())
        .collect();
    if qualifying_any.is_empty() {
        return Selection::OverCeiling;
    }
    let mut qualifying: Vec<(usize, &RosterEntry)> = qualifying_any
        .into_iter()
        .filter(|(_, r)| r.effective_enabled)
        .collect();
    if qualifying.is_empty() {
        return Selection::AllDisabled;
    }
```

Then change the three `.min()?` calls to `.min()` with an `expect` (the vec is now known non-empty), or restructure with `let Some(min_tier) = … else { return Selection::AllDisabled; }`. Prefer the latter — it never panics:

```rust
    let Some(min_tier) = qualifying.iter().map(|(_, r)| tier_rank(r.tier)).min() else {
        return Selection::AllDisabled;
    };
```

…and the same shape for `min_efficiency` and `min_dispatches`. Change the tail to:

```rust
    qualifying.sort_by_key(|(i, _)| *i);
    match qualifying.first() {
        Some((_, r)) => Selection::Chosen(r),
        None => Selection::AllDisabled,
    }
```

- [ ] **Step 6: Match on `Selection` in `route`**

At the `select_candidate` call in `route` (`src/triage.rs:342-357`), replace the `let Some(chosen) = … else { … }` with:

```rust
                let chosen = match select_candidate(
                    roster,
                    &routing,
                    repo_name,
                    repo_policy,
                    &dispatch_count_by_model,
                ) {
                    Selection::Chosen(r) => r,
                    Selection::OverCeiling => {
                        plan.flags.push(Flag::OverCeiling {
                            repo: repo_name.to_string(),
                            issue_id: issue.id.clone(),
                            complexity: routing.complexity,
                        });
                        continue;
                    }
                    Selection::AllDisabled => {
                        plan.flags.push(Flag::AllDisabled {
                            repo: repo_name.to_string(),
                            issue_id: issue.id.clone(),
                            tier: routing.tier_floor,
                        });
                        continue;
                    }
                };
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib triage::tests`

Expected: PASS. If any existing triage test constructs `Flag` exhaustively or matches on `select_candidate`'s old `Option`, update it to the new shape.

- [ ] **Step 8: Fix any non-exhaustive `Flag` matches**

Run: `cargo build 2>&1 | grep -A 3 'non-exhaustive'`

Expected: no output. If the reporter (`src/deck.rs` or `src/plan.rs`) matches `Flag` exhaustively, add an `AllDisabled` arm rendering it as e.g. `all-disabled: <repo>#<issue> (tier <tier> fully dark)`. Read the neighboring `OverCeiling` arm and mirror its format.

- [ ] **Step 9: Commit**

```bash
cargo fmt -- src/triage.rs
git add src/triage.rs
git commit -m "feat(triage): never select a disabled model; flag AllDisabled distinctly from OverCeiling"
```

---

### Task 5: The fallback walk skips disabled links

**Files:**
- Modify: `src/triage.rs` (add `CandidateRejection::Disabled`)
- Modify: `src/dispatch_cycle.rs` (the walk at `:492`, `next_eligible_roster` at `:680`, `record_remaining_ineligible` at `:692`)

**Interfaces:**
- Consumes: `RosterEntry.effective_enabled` (Task 3).
- Produces: `CandidateRejection::Disabled` — a **hard skip** (`record_fallback_skip`), not the bursar `Deferred` path. `Deferred` means "no link ran this cycle"; a skip means "this link is ineligible."

Note the boundary: we add a *variant* to the `CandidateRejection` enum, but the `candidate_rejection` **function** still does not check `enabled` (Task 4's M3 constraint holds). Only the walk constructs `Disabled`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/dispatch_cycle.rs`. Mirror the setup of the nearest existing `fallback`/chain test — read it first for the fixture helpers.

```rust
#[test]
fn next_eligible_roster_skips_disabled_links() {
    let mut primary = roster_entry_for_test("primary");
    primary.effective_enabled = false;
    let mut second = roster_entry_for_test("second");
    second.effective_enabled = false;
    let mut third = roster_entry_for_test("third");
    third.effective_enabled = true;
    let chain = vec![primary, second, third];
    let routing = routing_for_test();
    let got = next_eligible_roster(&chain, 0, &routing, CostPolicy::default());
    assert_eq!(
        got.map(|r| r.name.as_str()),
        Some("third"),
        "the walk must skip effective-disabled links"
    );
}
```

Substitute the real fixture-helper names from the surrounding tests; if none exist, build `RosterEntry` literals directly (all fields, including `enabled` and `effective_enabled`).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib dispatch_cycle::tests::next_eligible_roster_skips_disabled_links`

Expected: FAIL — returns `Some("primary")`, because `next_eligible_roster` only consults `candidate_rejection`.

- [ ] **Step 3: Add the `Disabled` rejection variant**

In `src/triage.rs`, add to `CandidateRejection` (`:177-181`):

```rust
    /// The row, or its provider, is toggled off. Constructed by the dispatch
    /// walk only — `candidate_rejection()` deliberately does NOT return this,
    /// so `select_candidate` can tell "dark" apart from "over-ceiling".
    Disabled,
```

Then find every exhaustive `match` on `CandidateRejection` (`cargo build` will name them; `record_fallback_skip` in `src/dispatch_cycle.rs:709` formats one) and add a `Disabled` arm. Mirror the neighboring arms' wording — e.g. `CandidateRejection::Disabled => "disabled".to_string()`.

- [ ] **Step 4: Skip disabled links in `next_eligible_roster`**

Replace the body (`src/dispatch_cycle.rs:685-690`):

```rust
    chain.iter().skip(start).find(|roster| {
        roster.effective_enabled
            && triage::candidate_rejection(roster, routing, repo_cost_policy).is_none()
    })
```

- [ ] **Step 5: Skip disabled links in the main walk**

In the chain loop (`src/dispatch_cycle.rs:492`), add the disabled check **before** the `candidate_rejection` check:

```rust
    for (idx, roster) in chain.iter().enumerate() {
        if !roster.effective_enabled {
            record_fallback_skip(
                report_path,
                item,
                roster,
                CandidateRejection::Disabled,
                fields,
            )?;
            continue;
        }
        if let Some(rejection) =
            triage::candidate_rejection(roster, &fields.routing, repo_cost_policy)
        {
            record_fallback_skip(report_path, item, roster, rejection, fields)?;
```

(Leave the rest of the loop body unchanged. `idx` keeps its existing use.)

- [ ] **Step 6: Skip disabled links in `record_remaining_ineligible`**

In `src/dispatch_cycle.rs:692-707`, record the disabled ones too:

```rust
    for roster in chain.iter().skip(start) {
        if !roster.effective_enabled {
            record_fallback_skip(
                report_path,
                item,
                roster,
                CandidateRejection::Disabled,
                fields,
            )?;
        } else if let Some(rejection) =
            triage::candidate_rejection(roster, routing, repo_cost_policy)
        {
            record_fallback_skip(report_path, item, roster, rejection, fields)?;
        }
    }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test`

Expected: PASS — 237/238 as baseline (only `harness_deck_validate`).

- [ ] **Step 8: Commit**

```bash
cargo fmt -- src/triage.rs src/dispatch_cycle.rs
git add src/triage.rs src/dispatch_cycle.rs
git commit -m "feat(dispatch): fallback walk skips effective-disabled links"
```

---

### Task 6: `config check` reports enabled counts per tier

Closes the loop: the operator can see a darkened tier without opening the TUI.

**Files:**
- Modify: `src/cli.rs:223` (the `config check` success line)

**Interfaces:**
- Consumes: `Config.roster`, `RosterEntry.effective_enabled`, `Config.providers`.

- [ ] **Step 1: Write the failing test**

`run_config_check` prints and returns an `ExitCode`, so test the pure helper instead. Add to `src/cli.rs` (create a `mod tests` if absent — check first):

```rust
#[test]
fn enabled_summary_counts_effective_enabled_per_tier() {
    let cfg = crate::config::parse_str(
        "[[provider]]\nname = \"p\"\nenabled = false\n\n[[roster]]\nname = \"a\"\ntier = \"lead\"\nceiling = \"XL\"\nefficiency = \"heavy\"\nbackend = \"agy\"\ndispatch_id = \"G\"\n\n[[roster]]\nname = \"b\"\ntier = \"senior\"\nceiling = \"M\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\nprovider = \"p\"\n",
    )
    .expect("config");
    assert_eq!(enabled_summary(&cfg), "lead 1/1 · senior 0/1 · junior 0/0");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib cli::tests::enabled_summary_counts_effective_enabled_per_tier`

Expected: FAIL — `cannot find function 'enabled_summary'`.

- [ ] **Step 3: Write `enabled_summary`**

Add to `src/cli.rs`:

```rust
/// `"lead 2/2 · senior 4/9 · junior 1/1"` — effective-enabled over total, per
/// tier. Surfaces a fully-dark tier at `config check` time.
fn enabled_summary(cfg: &crate::config::Config) -> String {
    [
        (Tier::Lead, "lead"),
        (Tier::Senior, "senior"),
        (Tier::Junior, "junior"),
    ]
    .iter()
    .map(|(tier, label)| {
        let rows = cfg.roster.iter().filter(|r| r.tier == *tier);
        let total = rows.clone().count();
        let on = rows.filter(|r| r.effective_enabled).count();
        format!("{label} {on}/{total}")
    })
    .collect::<Vec<_>>()
    .join(" · ")
}
```

Import `Tier` if `src/cli.rs` doesn't already (`use crate::config::Tier;`). `Tier` must derive `PartialEq` — it does (it's used in `matches!` comparisons); if the build complains, that's the fix.

- [ ] **Step 4: Print it**

At `src/cli.rs:223`, replace:

```rust
    println!("config: valid ({} roster entries)", cfg.roster.len());
```

with:

```rust
    println!(
        "config: valid ({} roster entries, {} providers)",
        cfg.roster.len(),
        cfg.providers.len()
    );
    println!("enabled: {}", enabled_summary(&cfg));
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cli::tests`

Expected: PASS.

- [ ] **Step 6: Verify against the live config**

Run: `cargo run -- config check --config conductor.toml`

Expected:
```
config: valid (24 roster entries, 7 providers)
enabled: lead 4/4 · senior 10/10 · junior 10/10
```
(Verified against the current roster: 4 lead, 10 senior, 10 junior = 24. Every model should be enabled, since nothing has been toggled off yet. If any count is short of its total, a provider seed name doesn't match a roster row's `provider` — fix the seed, not the count.)

- [ ] **Step 7: Full suite + commit**

Run: `cargo test`

Expected: 237/238 baseline.

```bash
cargo fmt -- src/cli.rs
git add src/cli.rs
git commit -m "feat(cli): config check reports enabled counts per tier"
```

---

## Phase 1 exit criteria

- [ ] `cargo test` at baseline (237/238; only `harness_deck_validate`).
- [ ] `conductor config check --config conductor.toml` reports 24 roster entries, 7 providers, all tiers fully enabled.
- [ ] Setting `enabled = false` on any `[[provider]]` block darkens its models — confirm by flipping `neuralwatt` and re-running `config check`, then reverting.
- [ ] A roster row naming an undeclared provider is a hard parse error.

## Reserved for the user (do not implement without asking)

`Flag::AllDisabled` is *raised* by Task 4, but what the cycle **does** with it is an operator decision the plan deliberately leaves open: skip the item and report it, escalate to the next tier up (which may spend money the operator was avoiding by darkening the lane), or hard-fail the cycle so the operator notices they've disabled a tier they still depend on. Task 4 pushes the flag and continues — the current, safest default. Raise the question before changing it.
