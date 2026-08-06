---
id: 0041
title: Persistent IMAP connection + CONDSTORE/QRESYNC flag-delta sync
type: perf
priority: later
status: open
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

## Prerequisite (added 2026-08-06)

[#0054](0054-schema-bump-bundle.md) must land first.
`sync_cursors.highest_modseq` currently holds a UID, not a modseq (`src/ingest.rs:606-614`), so scope item 3 above would issue `CHANGEDSINCE` with a UID-sized number, get an empty response and no error, and silently reproduce the [#0004](0004-fix-read-unread-sync.md) failure mode.

## Acceptance criteria

- Quick-sync `[TIMING]` drops sharply; no fresh LOGIN per op.
- CRITICAL (#0004): the non-CONDSTORE fallback keeps the full-window pass-1 FLAGS fetch; a webmail read/unread change on an old message still propagates on the next sync. Explicit regression test for both the CONDSTORE and the fallback path.
- Capability detection is defensive: advertise != correct; the heuristic fallback stays reachable.
- Proton Bridge CONDSTORE/QRESYNC capability probed and documented.

## Unblocks

- [#0042](0042-graph-delta-sync.md) (Graph delta shares the cursor + engine).
