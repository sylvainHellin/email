---
id: 0003
title: Move AccountState index scan to background thread
type: perf
priority: now
status: done
created: 2026-05-01
---

`AccountState::new()` blocks ~1.4 s scanning ~17k frontmatter files (TUM 1183 ms / Proton 234 ms / Work 1 ms) on TUI launch. This shows as a black screen between command and first frame.

## Fix

Move the index scan to a spawned thread; show "Indexing..." in the status bar until `BgResult::IndexReady` arrives. Block any sync action until the index is ready (or run sync against an empty index and let it self-populate -- TBD).

## Priority

P1 -- secondary to [#0002 persist-mailbox-states](0002-persist-mailbox-states.md). 1.4 s vs 14 s first-sync, but very visible because it blocks the first paint.

## Acceptance

- TUI shows its first frame within ~150 ms on a populated TUM account.
- Status bar shows progress while indexing.
- Sync actions queued before indexing completes work correctly when the index is ready.

## Resolution (2026-05-02)

- `AccountState::new` no longer scans mailboxes; it returns with
  `message_id_index` empty and `indexing: true`. Per-account
  `AccountState::new` cost dropped from 1183 ms / 234 ms to 13 ms / 4 ms
  on the local benchmark.
- The scan loop (with its `[TIMING]` log lines) was extracted into a
  free function `tui::app::types::build_message_id_index(mailboxes,
  account_name) -> MessageIdIndex`.
- `tui/mod.rs::run_loop` spawns one thread per account immediately
  after `App::new()` (mirroring the existing watcher fan-out),
  bumping `bg_count` so the existing spinner shows "Indexing...".
- New `BgResult::IndexReady { account_index, index }` variant is
  handled in `tui/bg.rs`: assigns the index to the account, clears
  `acct.indexing`, and emits an "Index ready" success status only when
  the last account finishes.
- Sync gating: chose the **queue-until-ready** branch of the TBD. No
  changes needed in `tui/actions.rs`; the existing `if app.bg_count > 0
  { app.queued_action = Some(...); }` guards in `Action::Fetch` and
  `Action::Sync` cover both user keypresses and watcher-driven
  `MailboxChanged -> push_action_dedup(Action::Fetch)`. Running against
  an empty index would have defeated the reconcile-skip from #0002.
- Mutations are not gated. Their race window with indexing is
  sub-second (faster than user reaction time); at worst a stale entry
  survives until the next reconcile.
- Tests: 3 new unit tests for `build_message_id_index` (per-dir
  collection, missing-dir skip, empty-mailboxes). Total 339 tests
  pass.

## Decision note (2026-08-14)

The owner confirmed the startup approach this ticket set in motion.
The TUI must paint instantly from stale or cached data, and a background refresh updates the UI when fresh data arrives.
That is the two-phase startup the perf audit still asks for beyond the index scan already shipped here: the first `terminal.draw` must not wait behind per-account store work.
The remaining piece is the per-account `PRAGMA integrity_check` and the redundant serial store opens that still gate the first paint (performance audit finding 1, §b.1 / S1, ~240 ms per 44 MB store, ~1.2 s for five accounts).
Scope is unchanged; this note records the confirmed direction so the follow-up work lands under the same ticket.
Priority stays now.
