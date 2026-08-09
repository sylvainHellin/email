---
id: 0043
title: FTS5 full-text search over the envelope + body store
type: feature
priority: later
status: done
created: 2026-07-14
---

Stage 5 (search) of the data-access-layer redesign. Plan: [data-access-layer](../plans/data-access-layer.md).

Full-text search becomes a `SELECT`, not a file stream.
This is the replacement for the dropped `grep`-the-tree affordance.
Depends on the store (Stage 2); best after the file layer is gone (Stage 4).

## Scope

1. FTS5 external-content virtual table `messages_fts` over subject / from / body-plaintext, fed at save/sync time from the store + blob bodies.
2. The `\` search path queries FTS5 instead of streaming files.
3. Keep the index fresh on sync writes; the store is the single freshness surface (no out-of-band file edits under the new model, so no reconcile-on-edit needed).

## Acceptance criteria

- Body/subject/from search returns correct hits via FTS5; ranked, fast on the full tree.
- Search latency `[TIMING]` well below today's file-streaming search.
- Index stays consistent with the store across sync and mutation.

## Shipped

`mp search --local <query>`: ranked, offline full-text search over the store's FTS5 index, covering every synced mailbox of the account at once, with `--mailbox` to narrow, `-n` to cap and `--full` to print bodies.
The server-side `mp search` keeps the command's default behaviour, so nothing that worked before changed.

Scope item 1 (the virtual table) and scope item 3 (freshness on sync writes) were already shipped by #0038.
`messages_fts` is contentless (`content=''`, `contentless_delete=1`), written inside the same transaction as the `messages` row and removed by every delete path: re-ingest of a UID, the UIDVALIDITY rebind, `store::write::delete_row`, `pending_ops::apply_delete` and the sync prune through `delete_by_uid`.
This ticket added the check that says so instead of the comment that claimed it, `store::search::index_drift`, and asserted it from the query side in `tests/store_search_integration.rs`.
There is no repair path and there cannot be one, because a contentless index has nothing to rebuild from; the store's drop-and-rebuild contract is the remedy.

Scope item 2 is **superseded**.
It says "the `\` search path queries FTS5 instead of streaming files", which was written before #0038; by the time this shipped, `\` no longer streamed files.
It is an incremental, case-insensitive *substring* filter over the loaded mailbox, and the rationale in `src/tui/app/types.rs` (the `SearchBodies` doc comment) states why it must not be served by `messages_fts`: FTS5 matches whole tokens and prefixes, which changes the result set for exactly the queries that mode exists for (a fragment inside a word, punctuation, a partial address), and the filter is OR-ed with the header fields and narrowed per keystroke, neither of which survives translation to a MATCH expression.
Implementing the sentence would have regressed a later, documented decision, so the `\` path is untouched and full-tree search is the CLI's.

What the query surface does with the user's typing, since none of this is FTS5's default:

- Every term becomes a double-quoted FTS5 string literal, so `c++`, `(draft)`, a trailing `AND` and a stray quote are searched as text rather than failing as syntax. Terms are whitespace-joined, which is FTS5's implicit `AND`.
- `"a phrase"` matches adjacency; a trailing `*` is a prefix; `subject:`, `from:` and `body:` restrict a term to one column, and an unrecognised `field:` is searched as the text it is.
- Ranking is `bm25(messages_fts, 10.0, 5.0, 1.0)`: a subject hit outranks the same word buried in a quoted reply chain.

## Verification

- `TMPDIR=$PWD/target/tmptest cargo test`: 1059 tests green (1032 before, +27).
- Live, read-only, against the local `assistant` store (712 messages): `mp -A assistant search --local -n 5 invoice` answers in 21 ms wall clock (`real 0m0.021s`), 4 hits across `archive` and `sent`. The pre-store affordance this replaces was a `grep` over the message tree.

## Acceptance criteria

- Body/subject/from search returns correct hits via FTS5; ranked, fast on the full tree. **Met** (`tests/store_search_integration.rs`, live run above).
- Search latency well below today's file-streaming search. **Met**: 9-21 ms for a whole-account search, and the file layer it is compared against no longer exists (#0040).
- Index stays consistent with the store across sync and mutation. **Met**, and now checkable: `index_drift` returns `(0, 0)` after re-ingest, rebind, move, delete and prune.
