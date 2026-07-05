# current-state.md — harness-conductor

Branch: main
`conductor-b9h` closed (`9029146` + `2d368b0`). `cargo test arena` passes 8/8. `cargo test` has 3 unrelated stale-expectation failures remaining.

## Plan

- [ ] Fix 3 stale-roster-count test failures so `cargo test` is fully green. Failures: `config::tests::checked_in_config_parses_and_has_seven_entries` (test name + assertion expect "seven entries" but conductor.toml now has 12 roster entries after neuralwatt lane was added), `roster_drift::tests::roster_drift_diff_fixture_agreement_against_real_config` and `roster_drift_diff_fixture_extra_in_config_against_real_config` (fixture expects 7 entries but real config has 12). Read the failing tests: `cargo test checked_in_config_parses_and_has_seven_entries -- --nocapture` and `cargo test roster_drift -- --nocapture`. Update the test expectation values + fixture to match the current roster count (12 entries: sonnet-5, opus-4.8, gpt-5.5, minimax-m3, qwen3.7-max, glm-5.2, gemini-3.5-flash, nw-glm-5.2, nw-glm-5.2-short, nw-glm-5.2-fast, nw-kimi-k2.6, nw-kimi-k2.6-fast). If a test name embeds "seven" consider renaming to a count-neutral name or updating the count. Do not change conductor.toml roster — update tests to match. Verify: `cargo test 2>&1 | tail -5`
