---
id: 0066
title: A drop-and-rebuild discards outbox rows and orphans blob files
type: bug
priority: next
status: done
created: 2026-08-06
---

Deferred note 6 from the fresh-context review of [#0054](0054-schema-bump-bundle.md) (commit `3d00aff`).
A pre-existing contract, but the v4 bump triggered it for every user at once, which is what makes it worth a ticket now.

## Evidence

- `remove_store_files` (`src/store/mod.rs:236-247`) deletes the sqlite file and its `-wal` / `-shm` sidecars, and nothing else.
  It runs on every version mismatch and every failed validation (`Store::open`, `src/store/mod.rs:68-93`).
- Consequence (a): `outbox` rows in `pending_send`, `sent_pending_append` and `failed` are discarded silently, although `mp outbox list|retry|discard` (`src/main.rs:525-610`) presents them as durable user-visible state.
  A message that was submitted but not yet appended to Sent, or one queued for a retry, simply stops existing, with no message to the user.
- Consequence (b): blob files on disk survive the rebuild with an emptied refcount table, so everything the refetch window does not bring back is an orphan, and no sweep is implemented to reclaim it (`src/store/blobs.rs:212` describes a sweep that does not exist).
- The module doc's framing, "a cache in front of IMAP, never a system of record" (`src/store/mod.rs:4-8`), is true for `messages` and false for `outbox`.
  The drop-and-rebuild contract was designed around the first and inherited the second.

## Scope

1. Decide what `outbox` is: either it stops being drop-and-rebuild collateral (carried across a rebuild, or persisted outside the versioned file), or the docs and `mp outbox` stop presenting it as durable.
   Record the decision here.
2. If it is carried: read the surviving rows before the drop and replay them into the fresh file, or keep the send queue in its own file with its own version stamp.
3. Handle the blob orphans: either delete the blob tree alongside the sqlite file (cheap, costs a refetch) or implement the sweep that `blobs.rs` already refers to.
4. Whichever way the blobs go, make the rebuild path say so in the log line it already emits.

## Decision (2026-08-06)

Scope item 1: the `outbox` stops being drop-and-rebuild collateral.
It is carried across the rebuild rather than persisted outside the versioned file, because a second file with its own version stamp is a second thing to keep in step for a table that holds a handful of rows at a time, and because the blob refcounts it depends on live in the store anyway.

How, in `src/store/rebuild.rs`, called from `Store::open`:

- Before the drop, the old `outbox` is read defensively: `SELECT *`, every column fetched by name, an absent or wrongly typed column yielding `None` rather than failing the row.
That is what makes the read safe against a schema that is not the current one, which is the whole point of running it on a file that just failed validation.
- Rows in `pending_send`, `sent_pending_append` and `failed` are written into the fresh file with a reference on the raw RFC822 blob they point at; `done` rows owe nothing and stay behind.
A row whose state is unreadable is carried as `failed` with the reason on `last_error`, never as something a driver would re-submit.
- A row that cannot be carried at all (its bytes are gone from the blob store, it names no account or message-id) is named in a `store-rebuild-<timestamp>.txt` note written next to the store, and in a `WARN` log line.
No silent discard.
- Scope item 3, the blobs: the whole tree is swept against the rebuilt `blobs` table, so every file with no refcount row is deleted, including a misplaced blob and a `.tmp` leftover from an interrupted write.
Deleting the tree wholesale was the alternative; the sweep is the same cost and is the thing that keeps the carried rows' own bytes alive.
The implemented sweep is deliberately rebuild-only: it is not the general reclaim `blobs.rs` still refers to, because outside a rebuild there is no moment where the store is known to be quiescent.
- Scope item 4: the log line the rebuild emits now counts what was carried, what was discarded and how many blob files were swept, and names the note file.

What this does not do: nothing here touches the send path, so the durability holes in [#0063](0063-send-durability-gaps.md) are unchanged.
A carried `pending_send` row for a Graph account is still unresumable, which is #0063 scope item 3.

## Review follow-up (2026-08-07)

The fresh-context review of `5d067e0` passed the change with one blocker and three consequential notes.
All four are fixed in the follow-up commit; the salvage now reads row by row rather than in one scan.

- Blocker, silent truncation: `rusqlite`'s `Rows::advance` resets the statement on a step error, so every later `next()` answers "no more rows".
  One `SELECT * FROM outbox` therefore ended for good at the first damaged page and the tail vanished with the note reporting nothing discarded (the review's probe carried 196 of 400 rows and named none of the 204).
  The salvage now lists the rowids, reads one row per query, and reads position by position past the point where the listing itself stopped; the same probe carries 372 of 400 and the note says what the remaining 28 positions cost.
  Whatever cannot be reached is counted against `COUNT(*)` and `MAX(rowid)` and named in the note.
- Note, the exactly-once marker failed open: a `submission_started_at` that was TEXT or REAL salvaged as `NULL`, which `sweep_pending_sends` reads as "the transport was never entered" and hands back to SMTP.
  A marker that is present but not an integer now parks the row as `failed`.
  A marker *column* that is absent still carries as empty: an outbox that predates the column (added mid-v2) recorded a marker for no row, so nothing in it distinguishes a queued submission from one that was inside an SMTP session, and parking every queued mail of every pre-marker store is a worse trade on a path each such store takes exactly once.
  Absence of the column is absence of evidence; a value that was written and cannot be read is evidence that cannot be dismissed.
- Note, symlinked blob root: `is_dir` and `WalkDir` both resolve a symlinked root even with `follow_links(false)`, so `blobs -> .` had the sweep delete the freshly rebuilt store file and its `-wal` / `-shm` from under the open handle.
  The sweep now stats the root with `symlink_metadata` and skips a symlink entirely, which also leaves a blob root a user parked on another disk intact.
- Note, unbounded salvage: a foreign database holding a large table named `outbox` produced a 12.6 MB note.
  A salvage reads at most 10 000 rows and says in the note when it stopped at that bound.

## Residual risks

- Carried blob bytes are stat'd, not re-hashed.
  A blob whose contents no longer match its name is carried with a fresh reference and `BlobStore::read` refuses it later, so the send fails loudly instead of the row being dropped quietly at rebuild time.
  That is the intended direction: a rebuild is not the place to decide a message is unsendable.
- The salvage reads a file that just failed `integrity_check`, and SQLite can return rows from a damaged page.
  A carried row could hold a corrupted `envelope` or `last_error`; a corrupted `state` or marker parks the row rather than re-sending it.
- The crash window is wider than "between the drop and the replay".
  A crash *inside* the per-row replay leaves some rows carried and the rest gone, and since the note is written after that loop, no note is left either.
  The next open finds a valid v4 store, does not rebuild, and there is no second chance.
  Milliseconds wide and the same kind of window as the documented one; closing it means writing the salvage to disk before the drop, which is the sidecar design this ticket rejected.
- The note files accumulate with no rotation, one per rebuild that touched the outbox.
  Their timestamp carries milliseconds, so two rebuilds of the same account in the same second no longer overwrite each other.
- A second `mp` process holding a healthy store on the same account while this one rebuilds loses its store file (pre-existing) and now its blob files too.
  Two processes rebuilding the same account at the same instant can both carry the same `pending_send` row and both submit it.
  The fix is the advisory lock of [#0061](0061-engine-advisory-lock.md).

## Acceptance criteria

- A store dropped for a version mismatch while an outbox row is in `pending_send` either preserves that row or tells the user it was discarded and why.
- After a rebuild, the blob directory holds no files without a refcount row, or a sweep exists that removes them.
- The module doc in `src/store/mod.rs` describes what actually survives a rebuild, table by table.

## Related

- [#0063](0063-send-durability-gaps.md) covers the other two durability holes in the same outbox.
