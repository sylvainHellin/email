---
id: 0072
title: Quick sync never notices a message archived in another client
type: bug
priority: now
status: done
created: 2026-08-07
---

Reported live: archive a few messages in another client (Gmail web, a phone), press `s` in the TUI, the status line says the sync is done and the archived messages are still sitting in the local inbox list.
The store stops mirroring the server for every removal the fetch window happens not to cover, and the user has no way to tell.

## Evidence

- Repro on `assistant` at `a81ca52`: `mp sync -A assistant -n 100`, then a standalone IMAP session (config credentials, no local store) moved `<5F84nR46Wn_0cIt7egm8vQ@notifications.google.com>` out of `INBOX`, then `mp sync -A assistant -n 100` again.
  Result: `Synced: 0 new, 281 already present`, no prune, and the inbox row (uid 1) still in `store.sqlite3`.
- The cause is the clamp in `vanished_uids` (`src/imap_client/fetch.rs`): the prune set was `known ∩ [min(window), max(window)]`, and the window is the last `limit` UIDs `UID SEARCH ALL` returned.
  A message archived elsewhere is *absent* from the listing, so it cannot pull the window's bottom down to itself; anything below `min(window)` is invisible to the diff forever.
  Archiving the oldest mail first, which is what everyone does, hits this every time: uid 1 vanished, the window became `[2, 83]`, and uid 1 fell outside it.
- The clamp was not wrong for what it had: it protected the rows below a short window from being deleted wholesale.
  But the fetch already holds the whole answer, `UID SEARCH ALL` returns the entire mailbox, and only the *download* is capped at `limit`.
  The diff was being computed over the capped half instead of the complete one.
- The Graph backend has had the complete-folder diff since #0065, with `pass_may_prune` as its safety gate.
  The IMAP path was the odd one out.

## Decision

Option (b) of the reported directions: full-mailbox liveness from the enumeration the fetch already performs.
No QRESYNC (option (a)) because `async-imap` exposes no `VANISHED` channel here and #0041 owns that work; no status-line-only mitigation (option (c)) because the enumeration is already paid for.

- `vanished_uids(known, listed, ceiling)` diffs the store's UIDs against the *whole* server listing, keeping only the top clamp: a known UID above `UIDNEXT - 1` is a locally-written placeholder (the `graph_uid` hash of an un-`APPENDUID`ed Sent copy) or a row written ahead of the server, never something the server forgot.
  Negative sentinel UIDs (a locally moved row) stay exempt.
- Two coverage flags gate the prune, mirroring Graph's `pass_may_prune` and now sharing it (moved to `ingest::pass_may_prune`):
  - *enumeration complete*: `UID SEARCH ALL` listed at least as many UIDs as `SELECT` said `EXISTS`.
    A truncated listing would read as a mass deletion.
  - *download complete*: every UID above the mailbox's local high-water mark was downloaded and ingested this pass.
    Backlog *below* the high-water mark does not count, because a quick sync deliberately never fetches it; what must not be missed is an arrival, which is what the copy of a message moved into this mailbox is.
- The prune runs through `prunable_uids` on the IMAP side too, so the age guard that protects a just-sent copy (#0065) covers both backends.
- Pruning stays a delete plus rediscovery in the destination mailbox, not a local move: the ordering that makes it safe (every target ingested, then every prune) is unchanged.

## Acceptance criteria

- Archiving a message in another client and running a quick sync removes the local inbox row. *(verified live on `assistant`, before/after)*
- A mailbox whose listing is short, or whose arrivals did not all land, prunes nothing that pass. *(unit tests)*
- A first sync, a UIDVALIDITY reset and a `graph_uid` placeholder still prune nothing. *(unit tests)*
- `cargo test` green, no new clippy warnings.

## Resolution

Shipped 2026-08-07.
Live: the same repro that left uid 1 in the inbox now reports `1 no longer in this mailbox` and the row is gone.

## Follow-up from the review of 4aca3c7

The arrival gate held for exactly one pass and then permitted the loss it exists to prevent.
It derived its mark from `max(known)` on every pass, so a bulk move of 300 messages into a mailbox whose quick sync takes the top 100 deferred correctly on pass 1 and then, on pass 2, stood on a high-water mark that its own ingest had raised above the 200 copies it never fetched: the gate opened, the source rows were pruned, and the positional window would never have gone back for the destination copies.
The mark is now persisted (`sync_cursors.arrival_mark`, schema v5) and carried into the next pass, which is held to the lower of the two.
It clears when a pass reaches through it, which any full sync does, and also when the missing arrivals stop being listed, so it cannot deadlock.

Two smaller holes closed with it.
`mp sync -n 0` computed a full vanished set and returned through the empty-window path, which reported the pass complete by construction: a prune-only pass with the gate forced open.
It now answers the coverage question like every other return.
And the two bounds on the deletion blast radius, `enumeration_complete` and the `ceiling` derivation, were inline in a function that needs a server to run; they are pure functions with unit tests now, including `EXISTS 0` and a server that reports no `UIDNEXT`.

### Gmail label semantics: the archived copy is re-filed by a full sync, not the next one

On a server where a move issues a fresh UID at the top of the destination folder (Exchange, Dovecot), one pass sees both halves: the source row is pruned and the destination copy is ingested by the same sync.
Gmail does not move anything.
Archiving removes the `INBOX` label, and the copy in `[Gmail]/All Mail` keeps the low UID it has had since it arrived, so it is not an arrival, the gate does not hold the pass back for it, and a capped quick sync will not fetch it: its UID sits far below the bottom of the archive window.
The result is correct but asymmetric.
The inbox row goes, which is the removal this ticket is about, and the archive copy is re-filed the next time the archive mailbox is synced in full (`S` in the TUI, `mp sync -n <huge>`).
Measured on the guinea pig account: All Mail uid 1 against a window bottom of 234.
This is the intended behaviour, not a deferral the gate can improve: a positional window cannot drain a backlog, which is what #0041 (CONDSTORE/QRESYNC) would change by making the pull a delta rather than a window.

## Follow-up from the sweep review: the first sync persisted a mark no pass could meet

Persisting the mark turned one pass of conservatism into a permanent one for every store that had just been created.
The mark is derived from what the mailbox is known to have held, an empty store knows nothing, and `high_water` answers `0` for an empty set, so the first capped sync of a mailbox bigger than the download window wrote `pending_mark = Some(0)`.
A mark of `0` says every UID the server lists must be in the store before the pass counts as complete, which a window of 50 or 100 never reaches on a mailbox of thousands, and the carried mark is combined with `min`, so it could not rise again.
`pass_may_prune` needs every target complete, so one such mailbox held the removals of the whole account, and schema v5 rebuilds every store while both entry points into a first sync are capped (`mp sync` defaults to `-n 50`, the TUI's quick sync to 100).
Every user of that build landed in "prunes off until a manual full sync" on first use.

Measured against the guinea pig account, both builds against the same live Gmail mailbox, each in its own data directory:

| | after the first `mp sync -n 50` | a synthetic vanished inbox row, next `mp sync -n 50` |
|---|---|---|
| the build being fixed | `arrival_mark` 0 on inbox, archive and sent | `⚠ 1 removal(s) held back` |
| this build | `arrival_mark` NULL on all three | `ℹ 1 message(s) left their mailbox on the server`, row gone |

### Why first contact is not an arrivals situation

The whole gate rests on the distinction between an arrival and a backlog: a quick sync deliberately never fetches the backlog, so measuring coverage against it reports every capped pass as short.
A mailbox nothing is known about has no arrivals, only backlog, so there is nothing to hand to the next pass.
The pass still reports itself short, which is one conservative pass and costs nothing: a store that has just been created has no rows to prune anyway, and the cursor that pass writes gives the next one a real line to stand on.

Clamping the persisted mark to the bottom of the download window would have been the wrong fix.
It is exactly the bulk-move hole this ticket's previous round closed: a mark of 100 against a window bottom of 301 lets pass 2 declare the 200 stragglers old news.

### The edge that keeps "no rows" from meaning "first contact"

A mailbox emptied in another client and then bulk-moved into holds no local rows either, and it must still defer.
What separates it from first contact is its cursor row, so the gate reads `sync_cursors.last_uid` as the floor when the rows are gone: the *absence* of a cursor row is first contact, a row recording a top of 100 means a UID above 100 is an arrival even against an empty store.
A recorded top is clamped to the same ceiling as the high-water mark, so a placeholder UID cannot lift the floor, and it is dropped along with the mark and the skip list across a UIDVALIDITY change, since all three are UIDs.

### Migration: the marks of `0` already on disk

Deriving the mark is fixed; the rows the previous build wrote are not, and there is no migrator (the store is a cache, so a version mismatch is answered by a rebuild).
They are cleared by a one-shot sweep, `UPDATE sync_cursors SET arrival_mark = NULL WHERE arrival_mark = 0`, run on the first open by a build that has the fix and stamped in `meta` (`arrival_mark_zero_swept`) so it never runs again.
One-shot rather than on every open, because a mark of `0` is still the right answer after the fix: a mailbox that had never held a message when it was last synced records a top of `0`, and a bulk move into it makes every copy an arrival.
A standing "a mark of 0 means nothing" rule would reopen the bulk-move hole for that mailbox for good.
The residual risk of the sweep is one mailbox in that state at the exact moment of the upgrade losing its deferral for one pass.

### Pins

- `a_first_capped_sync_of_a_large_mailbox_hands_on_no_mark` asserts the forbidden pre-fix answer (`Some(0)`) and that the next pass, standing on the cursor the first one wrote, prunes normally.
- `an_emptied_then_refilled_mailbox_defers_on_its_recorded_top` and `the_recorded_top_tells_first_contact_from_an_emptied_mailbox` pin the edge above, at the derivation and at the store round trip.
- `the_pass_after_a_bulk_move_still_defers_while_arrivals_are_missing` is unchanged in what it asserts.
- `any_persisted_mark_clears_once_a_pass_reaches_through_it` pins that no mark can get stuck, including `0`.
- `pre_fix_arrival_marks_of_zero_are_swept_once` pins both halves of the migration: the stuck mark goes, a real deferral does not, and a mark written after the sweep survives the next open.
