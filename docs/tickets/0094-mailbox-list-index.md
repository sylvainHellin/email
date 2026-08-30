---
id: 0094
title: Index the mailbox listing sort and drop the correlated per-row subquery
type: perf
priority: next
status: done
created: 2026-08-14
---

## Resolution (2026-08-14, schema v7)

Shipped both scope items 1 and 2. Item 3 (windowed LIMIT) left out: not needed to
meet the acceptance criteria and out of the narrow scope taken here.

- Added `CREATE INDEX messages_list ON messages (account, mailbox, date_sort DESC, id DESC)`
  in `src/store/schema.rs` and bumped `SCHEMA_VERSION` 6 -> 7. There is no
  migrator: a v6 store is dropped and rebuilt from its cache on the next open,
  and a fresh store carries the index from `SCHEMA_SQL`.
- Rewrote `read::list_mailbox` (`src/store/read.rs`) to source the invite flag
  from a `LEFT JOIN message_blobs` instead of the correlated per-row `EXISTS`
  in `row_columns()`; the other listing functions keep `row_columns()`
  unchanged (out of scope). The join goes through a `SELECT DISTINCT`
  subquery so it stays one-to-at-most-one even if a message carries several
  attachments named `invite.ics` (review finding, fixed before commit).
- Verified with EXPLAIN QUERY PLAN on a 5000-row synthetic mailbox:
  - Before: `SEARCH messages USING INDEX sqlite_autoindex_messages_1` +
    `CORRELATED SCALAR SUBQUERY` + `USE TEMP B-TREE FOR ORDER BY`.
  - After: `SEARCH messages USING COVERING INDEX messages_list` +
    `SEARCH invite ... LEFT-JOIN`, no temp B-tree, no correlated subquery.
- Regression test `store::read::tests::the_listing_is_served_by_the_messages_list_index`
  asserts the plan uses `messages_list` and carries no temp B-tree or
  correlated subquery, against the exact SQL `list_mailbox` runs.

The mailbox listing sorts without an index and pays a correlated subquery per row (performance audit §b.4, confidence 0.7).
`load_emails` -> `read::list_mailbox` (`src/store/read.rs:169`) runs `... WHERE account=?1 AND mailbox=?2 ORDER BY date_sort DESC, id DESC`, selecting `row_columns()` which includes an `EXISTS (SELECT 1 FROM message_blobs ...)` per row (`read.rs:137`).
The only usable index is `UNIQUE(account, mailbox, uid)` (`src/store/schema.rs:222`); there is no index on `(account, mailbox, date_sort)`, so SQLite filters via the unique index and then does a temp-B-tree sort on `date_sort`.
There is no LIMIT, so the whole mailbox is materialised per load, and mutations reload the current mailbox.

Negligible at a few hundred rows; it grows into perceptible lag on large mailboxes and repeats on every reload.

## Scope

1. Add `CREATE INDEX messages_list ON messages(account, mailbox, date_sort DESC, id DESC)` in `src/store/schema.rs` so the `ORDER BY` is index-served, with a schema-version bump.
2. Remove the correlated `EXISTS` per row in `list_mailbox`; source the has-blob signal without a per-row subquery (join or a stored column), so the listing is one index scan.
3. Consider a windowed LIMIT for very large mailboxes.

## Acceptance criteria

- The mailbox listing is served by an index scan rather than a temp-B-tree sort, flat in mailbox size for the visible window.
- No correlated subquery runs per row on a list load.
- The schema-version bump follows the store's drop-and-rebuild contract; a fresh store carries the new index.
