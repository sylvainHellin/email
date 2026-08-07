---
id: 0005
title: Parallel IMAP fetch per mailbox
type: perf
priority: next
status: done
created: 2026-05-01
---

`sync_mailboxes` uses one IMAP session and SELECTs mailboxes sequentially (IMAP requires one selected mailbox per connection). For accounts with 3+ mailboxes on a remote server, each SELECT+SEARCH+FETCH cycle adds ~200-300 ms of network latency serially.

## Fix

Open N parallel IMAP connections (one per mailbox) so `N * latency` becomes `1 * latency`.

## Trade-off

N TLS handshakes + N logins instead of one. For small N (3-5 mailboxes) the extra handshake cost is < the latency saved.

## Sequencing

Measure post-[#0002 persist-mailbox-states](0002-persist-mailbox-states.md) and [#0003 cold-start](0003-cold-start-async-indexing.md) to confirm this is still the dominant cost.

## Adaptations from the original plan

The ticket predates the store cutover (#0038/#0050/#0052) and the #0072 prune/arrival-mark machinery.
The sequential loop it describes now lives in `sync_mailboxes` in `src/imap_client/store_sync.rs`, not the old `.md`-writing orchestrator, and the loop body carries the coverage gate, arrival marks and the deferred second prune pass.

The implemented shape keeps those invariants untouched by splitting the loop into three phases rather than parallelising it in place:

1. read every target's skip list from the store, serially (single-reader, cheap);
2. fetch every mailbox in parallel, one IMAP session each, capped by `imap.fetch_concurrency` via `futures::stream::buffered`;
3. ingest serially in target order, exactly the old loop body.

Only phase 2 is concurrent, and it touches no SQLite, so the single-writer discipline holds.
`buffered` (not `buffer_unordered`) preserves target order, so inbox-before-archive-before-sent and the whole #0072 prune gate are byte-for-byte the old behaviour.
The concurrency cap is a per-account config value, `[accounts.imap] fetch_concurrency`, default 4, clamped to [1, 8]; 1 restores the single-session path.
It honours the existing 30s connect timeout on `open_imap_session` unchanged (each parallel session opens through it).

Live smoke (3 mailboxes each, warm store): `assistant` (Gmail) ~1.9s -> ~0.7s, `tum` ~2.5s -> ~1.3s.

Known edge (from the post-ship review): phase 2 collects every mailbox's full fetch window before ingest, so peak memory on a large initial sync scales with mailbox count times the `-n` window, where the old path held one mailbox at a time.
A bounded fetch-to-ingest pipeline would cap it if huge initial syncs ever matter.
With `fetch_concurrency = 1` the ordering is the old one, but it still opens one session per mailbox in turn rather than a single shared session; connection churn only, correctness identical.
