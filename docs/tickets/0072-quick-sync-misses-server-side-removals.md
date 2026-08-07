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
