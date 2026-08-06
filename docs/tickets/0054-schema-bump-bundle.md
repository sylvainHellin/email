---
id: 0054
title: Schema bump bundle (modseq/UID split, pending_ops.updated, account convention, dead columns)
type: refactor
priority: now
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 0 item 2 plus Tier 2 item 5 and Tier 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M.

Four schema corrections that are individually small and all want the same version bump, so they ship as one migration rather than four.
The first of them is a correctness trap for [#0041](0041-persistent-conn-condstore.md), which is why this ticket is a prerequisite for that one.

## Evidence

- `src/ingest.rs:606-614` binds `cursor.last_uid` into the `highest_modseq` column of `sync_cursors`, so the value stored is a UID rather than a modification sequence.
  Harmless while nothing reads it as a modseq; once #0041 issues `CHANGEDSINCE <modseq>` with a UID-sized number, the server returns nothing and no error, reproducing the [#0004](0004-fix-read-unread-sync.md) failure mode silently.
- `src/store/schema.rs:214-222` `pending_ops` has `created` but no `updated`, and the backoff formula #0039 needs is a function of the last attempt, not of creation.
- `src/store/schema.rs:207-212` `sync_cursors` is keyed on `mailbox` alone, and `pending_ops` has no `account` column, while `mailboxes`, `messages`, `drafts` and `outbox` all carry `account`. Stores are per-account today, so nothing is broken; the convention is still inconsistent and one of the two shapes should win explicitly.
- `src/store/schema.rs:148` `messages.mtime` and `src/store/schema.rs:123` `mailboxes.unread_count` are written and never read back.

## Scope

1. Split `sync_cursors.highest_modseq` into `last_uid INTEGER` and a `highest_modseq INTEGER` that only ever holds a real modseq.
   Update the write at `ingest.rs:606` and the read at `ingest.rs:620`.
2. Add `pending_ops.updated INTEGER`.
3. Settle the account-column convention: either every table carries `account`, or the per-account-store invariant is documented once in `src/store/schema.rs` and the redundant columns come out.
   Whichever wins, `sync_cursors` and `pending_ops` follow it.
4. Drop `messages.mtime` and `mailboxes.unread_count`, and the writes that populate them.
5. Bump `SCHEMA_VERSION` once for the bundle.
   Existing stores are rebuilt by sync, so a drop-and-recreate on version mismatch is acceptable if that is the established path; state the choice in the ticket when it is made.

## Acceptance criteria

- Nothing writes a UID into a modseq column, and a grep for `highest_modseq` shows only modseq values on both sides.
- `cargo test` green; an opened store with the previous version is handled by the documented mismatch path rather than by an unhandled error.
- `docs/architecture.md` (see [#0056](0056-architecture-docs-rewrite.md)) and the schema doc comment describe the new columns.

## Blocks

- [#0041](0041-persistent-conn-condstore.md) must not open before the modseq split lands.
- [#0039](0039-pending-ops-queue.md) wants the `updated` column and the account convention decided first.
