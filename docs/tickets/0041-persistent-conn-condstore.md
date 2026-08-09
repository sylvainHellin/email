---
id: 0041
title: Persistent IMAP connection + CONDSTORE/QRESYNC flag-delta sync
type: perf
priority: later
status: done
created: 2026-07-14
---

Stage 5 (IMAP) of the data-access-layer redesign. Plan: [data-access-layer](../plans/data-access-layer.md).

The smoothness win: stop paying per-op TCP+TLS+LOGIN and stop full-window flag fetches where the server supports deltas.
This rewrites the "one session per operation" invariant (`docs/architecture.md:23`), which the owner approved.

Capability reality (see [server-capability-matrix](../../.agents/research/2026-07-14-server-capability-matrix.md)): Proton Bridge supports neither CONDSTORE nor QRESYNC (heuristic + IDLE only), Gmail supports CONDSTORE not QRESYNC, only Dovecot (tum) supports the full QRESYNC ladder. So QRESYNC benefits exactly one account; the heuristic + IDLE path (Proton, the daily driver) must stay first-class.
async-imap 0.11.2 surface is confirmed (see plan "Resolved unknowns"): typed `select_condstore()`, parser decodes `HIGHESTMODSEQ`/`MODSEQ`/`VANISHED (EARLIER)`, and `run_command`/`read_response` are public for the QRESYNC/ENABLE raw path.

## Scope

1. Small spike: confirm the `UID FETCH ... (FLAGS) (CHANGEDSINCE <modseq>)` fetch-modifier string form against async-imap's fetch API (the only unconfirmed detail).
2. Persistent authenticated connection per account: a long-lived engine-owned session for sync + mutations, plus a second connection for IDLE (IDLE blocks its own connection). Reconnect with backoff, `NOOP` keepalive, re-`SELECT` and resume from the stored cursor. Update `docs/architecture.md` in the same change.
3. CONDSTORE flag-delta: persist HIGHESTMODSEQ per mailbox in `sync_cursors`; quick sync does `FETCH (FLAGS) (CHANGEDSINCE <modseq>)`. Capability-gated.
4. QRESYNC where advertised: `SELECT ... (QRESYNC (uidvalidity modseq))` folds vanished + changed into SELECT. Gate strictly on CAPABILITY; fall back to CONDSTORE, then to the current heuristic.
5. UIDPLUS for own writes (APPEND/COPY): capture the returned UID to update the store without a follow-up search.

## Spike answer: the CHANGEDSINCE fetch-modifier form (scope 1, resolved 2026-08-09)

Read off `async-imap` 0.11.2 source (`client.rs:472`) and `imap-proto` 0.16 (`parser/rfc4551.rs`).

`Session::uid_fetch(uid_set, query)` builds the command by plain interpolation:

```rust
self.run_command(&format!("UID FETCH {} {}", uid_set.as_ref(), query.as_ref()))
```

There is no validation, escaping or parenthesising of `query`, so the RFC 7162 fetch modifier is simply the tail of the `query` argument:

```rust
session.uid_fetch("1:*", "(UID FLAGS) (CHANGEDSINCE 90060115205545359)").await
// wire: UID FETCH 1:* (UID FLAGS) (CHANGEDSINCE 90060115205545359)
```

which is exactly the `fetch-modifiers` production of RFC 7162 s3.1.4. No API addition is needed and nothing has to be dropped down to `run_command`.

Two supporting facts confirmed at the same time:

- the response parser accepts the `MODSEQ (n)` data item CONDSTORE adds to every `FETCH` reply (`imap-proto` `parser/rfc4551.rs:34`), so the extra item does not break the existing `(UID FLAGS)` decode; `async_imap::types::Fetch` exposes no accessor for it, but this ticket does not need per-message modseqs, only the mailbox `HIGHESTMODSEQ`;
- `Session::select_condstore()` issues `SELECT <mbox> (CONDSTORE)` and `parse_mailbox` fills `Mailbox::highest_modseq: Option<u64>` from the `OK [HIGHESTMODSEQ n]` response code (`client.rs:352`, `client.rs:1783`).

The wire form is pinned by a unit test on the command builder rather than by a live server, because `async_imap::mock_stream` is a private module (`lib.rs:105`) and `ImapSession` is monomorphic over the crate's own TLS `ImapStream`, so there is no seam to script a fake session through without a wider refactor.

## Prerequisite (added 2026-08-06)

[#0054](0054-schema-bump-bundle.md) must land first.
`sync_cursors.highest_modseq` held a UID, not a modseq, so scope item 3 above would issue `CHANGEDSINCE` with a UID-sized number, get an empty response and no error, and silently reproduce the [#0004](0004-fix-read-unread-sync.md) failure mode.
Cleared: #0054 shipped as schema v4, `last_uid` and `highest_modseq` are separate columns and the latter is NULL until this ticket writes a real modseq into it.

## Carry-forward hazard in the cursor UPSERT (added 2026-08-06)

From the fresh-context review of #0054 (commit `3d00aff`), deferred note 4.
`record_mailbox_cursor`'s UPSERT sets `highest_modseq` and `deltalink` unconditionally from the caller's struct (`src/ingest.rs:602-606`), and both backends pass `None` on the full-window path (`src/graph.rs:682`, `src/imap_client/store_sync.rs:216`).
The moment scope item 3 below writes a real modseq from the CONDSTORE path, the next full sync wipes it, and the ticket's own failure mode returns: a NULL modseq falls back to the full window, silently, with no error.
The same trap is already latent for `deltalink` and [#0042](0042-graph-delta-sync.md).

Fix it here, before writing the first modseq: load the existing cursor and carry the value forward, or write `COALESCE(excluded.highest_modseq, highest_modseq)` (and the same for `deltalink`) into the `ON CONFLICT` clause.
A test that records a modseq, then records a full-window cursor with `None`, then asserts the modseq survived, pins it.

## Constraint from #0059 (added 2026-08-09)

`SyncBackend::fetch_targets` is an AFIT (native async-fn-in-trait, no `async-trait` crate), so the returned future carries no `Send` bound.
Every current caller awaits it in place (`main.rs`, `tui/helpers.rs`), which is fine; a persistent-session implementation that wants to `tokio::spawn` a sync must first add a `Send` bound or box the future.

## Acceptance criteria

- Quick-sync `[TIMING]` drops sharply; no fresh LOGIN per op.
- CRITICAL (#0004): the non-CONDSTORE fallback keeps the full-window pass-1 FLAGS fetch; a webmail read/unread change on an old message still propagates on the next sync. Explicit regression test for both the CONDSTORE and the fallback path.
- Capability detection is defensive: advertise != correct; the heuristic fallback stays reachable.
- Proton Bridge CONDSTORE/QRESYNC capability probed and documented.

## Outcome (2026-08-09)

Shipped: scope 1 (the spike, answered above), the cursor carry-forward fix, scope 2 (the persistent session pool) and scope 3 (the CONDSTORE flag delta).

Split into [#0081](0081-qresync-uidplus.md): scope 4 (QRESYNC) and scope 5 (UIDPLUS), neither of which fell out of the work above, plus one limitation of the shipped delta that is honest to record rather than hide.

Deviations from the scope as written, and why:

- Scope 2 asked for "a long-lived engine-owned session".
It is a pool in `src/imap_client/pool.rs` rather than a session on `ImapBackend`, for two reasons that only became visible while building it.
IMAP allows one SELECTed mailbox per connection, so the #0005 parallel per-mailbox fetch genuinely needs several at once and a single owned session would have serialised it; and the sessions have to outlive the backend, which is constructed fresh per `sync_mailboxes` call and does not exist at all on the queued-op drain path the ticket also wanted to cover.
The pool covers sync, ops, batches and searches; the `&mut self` slot #0059 built is still there and still unused.
- The second connection for IDLE needed no work: `watch.rs` already opens its own session and is deliberately left unpooled, which is the shape the ticket described.
- The AFIT constraint from #0059 was not hit: nothing here spawns a sync, so no `Send` bound was needed.
- A CONDSTORE resume point is recorded only by a pass whose window covered the whole mailbox.
The reasoning is in `modseq_to_record`'s doc comment; the cost, that the delta only starts working after a full sync on a large mailbox, is item 1 of #0081.

## Unblocks

- [#0042](0042-graph-delta-sync.md) (Graph delta shares the cursor + engine).
