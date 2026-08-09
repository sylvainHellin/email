---
id: 0081
title: QRESYNC, UIDPLUS, and advancing the modseq on a capped pass
type: perf
priority: later
status: open
created: 2026-08-09
---

The half of [#0041](0041-persistent-conn-condstore.md) that was split out rather than shipped. #0041 delivered the persistent session pool, the strict capability gate and the CONDSTORE flag delta; scope items 4 and 5 (QRESYNC, UIDPLUS) did not fall out naturally from those, and one limitation of the CONDSTORE path is recorded here rather than hidden.

The order below is by value: item 1 is worth more than the other two combined, because it is what makes the shipped delta actually run on the common path.

## 1. Advance the modseq on a capped pass

`imap_client::fetch::modseq_to_record` records a `HIGHESTMODSEQ` only when the pass's window covered every UID `UID SEARCH ALL` listed. The reasoning is in its doc comment and is sound: a stored modseq claims "every flag in this mailbox was correct as of `n`", and a quick sync that saw the last 50 of 8000 UIDs cannot claim it.
Recording one anyway would tell the next full sync to skip exactly the old-message flag changes it exists to catch, which is [#0004](0004-fix-read-unread-sync.md).

The consequence is that on a mailbox larger than the quick-sync window, the delta only begins working after a full sync, and quick syncs then consume the resume point without advancing it, so the `CHANGEDSINCE` set grows until the next full sync.
Correct, but it leaves most of the win on the table.

The fix is a second cursor column: the UID range the recorded modseq is valid for.
A pass records `(modseq, covered_from_uid)`, and a later pass may issue `CHANGEDSINCE` for the part of its window at or above `covered_from_uid` while doing the full flag fetch below it.
That is a schema change, so it wants the next schema bump rather than a migration.

## 2. QRESYNC (Dovecot only)

`SELECT ... (QRESYNC (uidvalidity modseq))` folds the vanished set and the changed flags into the SELECT response, which would replace the `UID SEARCH ALL` enumeration on the one account that can use it.

Only `tum` (Dovecot) advertises it; Gmail never implemented it and Proton Bridge (Gluon) does not have it either, so it benefits exactly one account, which is why it lost to the rest of #0041 on effort.
`async-imap` 0.11.2 has no typed `select_qresync`, so it needs the raw `run_command`/`read_response` path plus `ENABLE QRESYNC`, and the `VANISHED (EARLIER)` response has to be reconciled with the existing `vanished_uids` diff and its #0072 coverage gate.
That reconciliation, not the wire format, is the real work: the prune gate currently rests on "the listing accounted for every message SELECT announced", and a QRESYNC pass never produces a listing.

The capability gate already exists and already reads QRESYNC (`imap_client::pool::ServerCaps`), and is tested to be off for Gmail and Proton.

## 3. UIDPLUS for our own writes

Capture `APPENDUID` / `COPYUID` so an APPEND or COPY updates the store row's UID directly instead of leaving a placeholder for the next sync to bind through the Message-ID (`ingest::graph_uid`, and the "written optimistically ahead of the server" rows the prune ceiling exempts).

All three real IMAP servers advertise UIDPLUS, so unlike QRESYNC this one is broadly useful; it is out of #0041 only because its call sites are the APPEND path (`imap_client::sent`) and the batch COPY, neither of which #0041 touched. `ServerCaps::uidplus` is already read and already gated.

## Not in scope

The heuristic + IDLE path stays first-class and untouched, exactly as in #0041: it is what Proton Bridge, the daily driver, runs on.
