---
id: 0066
title: A drop-and-rebuild discards outbox rows and orphans blob files
type: bug
priority: next
status: open
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

## Acceptance criteria

- A store dropped for a version mismatch while an outbox row is in `pending_send` either preserves that row or tells the user it was discarded and why.
- After a rebuild, the blob directory holds no files without a refcount row, or a sweep exists that removes them.
- The module doc in `src/store/mod.rs` describes what actually survives a rebuild, table by table.

## Related

- [#0063](0063-send-durability-gaps.md) covers the other two durability holes in the same outbox.
