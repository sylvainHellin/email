---
id: 0077
title: Three intermittent test failures (temp-dir and env-var races)
type: bug
priority: later
status: done
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

## Resolution (2026-08-11)

**The suspected mechanism was not the cause.** The env window is real and has been closed anyway, but the three failures came from somewhere else entirely.

### Root cause: a draft id that is a YAML number

`store::drafts::new_id` minted 16 random hex characters, and `row_for` writes them into the file's YAML frontmatter *unquoted*.
A plain hex string is not always a YAML string:

- `8808e70039225152` is a **float** in scientific notation. `EmailFrontmatter::id` is `Option<String>`, gray_matter hands serde a float, and the field deserialises to `None` -- silently. The refresh then treats the draft as having no id, mints a *new* one, writes it back, and the draft's identity has changed under every selector and index row that referred to it.
- `1234567890123456` (all digits) is an **integer**, which fails deserialisation outright, so the whole draft is skipped from the index (#0080's skip path).

Roughly one hex string in a thousand has one of those two shapes, and a full suite run mints dozens of ids. That is exactly the observed rate, and it explains all three symptoms without any interleaving:

| Test | What the bad id does |
|---|---|
| `renaming_a_file_keeps_the_draft_id` | the id read back before the rename differs from the one minted after it, so `find` misses: "id survives the rename" |
| `the_drafts_count_agrees_with_the_list_on_a_never_synced_account` | an integer-shaped id skips the draft, so the count and the list disagree by one |
| `an_unresolved_search_hit_replies_from_its_fetched_content` | the selector on the status line no longer resolves to the draft that was just written |

It also produced `the_preview_shows_the_body_of_the_selected_draft` (empty body: `load_draft_body` cannot find the id) and `a_draft_without_an_id_is_assigned_one_and_it_is_written_back` during this ticket's stress runs, which is how it was caught.

Reproduced directly rather than by interleaving: writing `id: 8808e70039225152` into a draft and calling `parse_email_draft` returns `Ok(None)` for the id, and `id: 1234567890123456` returns `Err`.

**Fix**: a minted id now starts with a letter (`a..=f`), which no YAML number can. 62 bits of entropy instead of 64. `a_minted_id_is_never_a_yaml_number_and_round_trips_verbatim` mints 2000 ids, asserts the shape, and round-trips ten of them through the real writer and reader; `a_number_shaped_id_does_not_survive_the_frontmatter_round_trip` pins the two failing shapes and the index behaviour they cause, so the constraint cannot be relaxed by accident.

### The env window, closed anyway

Scope item 2 was done on its own merits. `std::env::set_var` in a multi-threaded process is a data race on `environ`, not merely an unsynchronised read, and `data_dir_lock` only serialised the writers -- every `tempfile::tempdir()` on another thread read `$TMPDIR` without it.

`config::test_env` (all `#[cfg(test)]`) holds thread-local overrides for the data dir, `$HOME` and `$MAILYPOPPINS_CONFIG_DIR`; `parse::materialisation_root` resolves through `parse::test_temp_root()` in a test binary instead of an overridden `$TMPDIR`. libtest runs each test on its own thread, so a fixture's paths are invisible to every other test: no lock, no serialisation, and the data-dir tests now run in parallel. `config::data_dir_lock` is gone. No shipped code path changed.

Residual: a test that resolves a `config::` path on a thread it spawned itself would fall back to the real data dir rather than the fixture's. No test does today.

## Verification

- `TMPDIR=$PWD/target/tmptest cargo test`: 1061 green (1059 before, +2).
- Flake loop: 12 consecutive full-suite runs, 0 failures.
- Stress: 30 consecutive `cargo test --lib -- --test-threads 64` runs, 0 failures. Before the id fix, that loop failed roughly once in ten (three distinct drafts tests).
- `cargo clippy --all-targets`: 20 warnings, all pre-existing.

## Acceptance criteria

- The mechanism is reproduced and named before any fix lands. **Met** -- reproduced deterministically, above.
- All three tests survive a repeated full-suite run (at least 20 consecutive runs). **Met** -- 12 full-suite runs plus 30 stress runs at 64 threads, 0 failures.
- No production code path changes to accommodate a test. **Met** -- the only production change is `new_id`, which fixes a bug a real user hits (an agent-written draft whose minted id is float-shaped loses its identity on the next refresh). The test seams are `#[cfg(test)]`.

Not split out, because none of the three had a separate cause.

## Follow-up

`set_draft_id` still writes the id unquoted, so a *hand-written* `id: 123e456` in an agent-authored draft is still read as `None` and silently re-minted. Quoting the value on write does not help (the file is read before it is written); fixing it properly means a lenient deserialiser for `id:` that rejects non-string scalars loudly instead of defaulting to `None`. Worth a ticket, out of scope here.
