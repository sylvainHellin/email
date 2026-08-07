---
id: 0077
title: Three intermittent test failures (temp-dir and env-var races)
type: bug
priority: later
status: open
created: 2026-08-08
---

Three tests fail intermittently and pass on a re-run.
None of them is tied to the change that was in flight when it was seen, and all three have been observed on more than one commit.
Effort: S per test, once the mechanism is confirmed.

| Test | Where | Seen |
|---|---|---|
| `store::drafts::tests::renaming_a_file_keeps_the_draft_id` | `src/store/drafts.rs:433` | #TKT-0051 review of `04d311c`: failed on the first full run ("id survives the rename"), passed on three further full runs and on an isolated run |
| `tui::app::types::tests::the_drafts_count_agrees_with_the_list_on_a_never_synced_account` | `src/tui/app/types.rs:1878` | recorded in [the 2026-08-07 handoff](../../.agents/handoff/2026-08-07T09-21-25Z-smart-compact-1-compaction-handoff.md) |
| `tui::actions::tests::an_unresolved_search_hit_replies_from_its_fetched_content` | `src/tui/actions.rs:3116` | same handoff, noted there as an env-var race |

Not reproduced in this sweep: five consecutive `cargo test --lib` runs, 785 passed each time.
A flake that needs load or an unlucky interleaving is expected to survive that.

## Suspected mechanism

`tui::actions::tests::Fixture` mutates two process-wide environment variables, `MAILYPOPPINS_DATA_DIR` and `TMPDIR`, and restores them on drop (`src/tui/actions.rs:2812-2884`).
The `data_dir_lock()` guard it holds serialises fixtures against each other, but not against every other test in the binary: a test that only calls `tempfile::tempdir()` reads `$TMPDIR` without taking that lock, so which directory it lands in depends on where a concurrent fixture happens to be in its set / restore window.
The comment at `src/tui/actions.rs:2790-2809` already names the failure that follows from it: a fixture deleting its own tree can pull another test's directory out from under it.

`renaming_a_file_keeps_the_draft_id` fits that shape.
It creates a `TempDir`, writes a draft, indexes it (which mints an `id:` and writes it back into the file), renames the file and re-indexes; a directory that vanishes mid-test, or a write-back that lands somewhere the second scan does not look, produces exactly the observed "id survives the rename" failure.
This is a hypothesis, not a diagnosis: it has not been reproduced under a debugger or with a forced interleaving.

## Scope

1. Confirm the mechanism first, by looping the suite under `--test-threads` pressure or by instrumenting the fixture's env window; do not fix what has not been reproduced.
2. If it is the env window, take the process-wide mutation out of it: resolve the data dir and the temp dir through a value the fixture passes down rather than through `std::env::set_var`, so no test can observe another's setting.
   Every reader of `MAILYPOPPINS_DATA_DIR` and `$TMPDIR` inside the test binary has to go through that seam for it to help.
3. If any of the three turns out to have its own unrelated cause, split it into its own ticket.

## Acceptance criteria

- The mechanism is reproduced and named before any fix lands.
- All three tests survive a repeated full-suite run (at least 20 consecutive runs) after the fix.
- No production code path changes to accommodate a test.
