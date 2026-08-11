---
id: 0085
title: On-open re-fetch of an evicted body (the missing half of #0060)
type: feature
priority: later
created: 2026-08-07
status: open
---

The retention sweep shipped in [#0060](0060-retention-enforcement.md). It evicts
cached blob files while keeping every `messages` row, on the promise recorded at
`src/config.rs` (the `RetentionPolicy` docs, "bodies are re-fetched on open"):
the store is a cache, so an evicted body is not lost, only not-here-right-now.

That promise is not yet kept. Opening a message whose body blob was evicted
shows an empty body, and a plain `mp sync` does **not** bring it back: the sync
skips any UID it already holds a `messages` row for (`ingest::known_uids`, feeding
both the download-skip and the vanished/prune sets in `src/imap_client/fetch.rs`).
So today the only recovery is a targeted re-ingest of that one message.

**This is the missing half of #0060's acceptance criterion** "opening an evicted
message re-fetches and re-materialises its body", and it is **required before
anyone lowers a `max_disk_bytes` cap below their working set** — until it ships,
`mp store gc` refuses to reclaim more than half a store at once without `--force`.

## Why not just make the sync skip-list blob-aware

Rejected on record (#0060 sign-off). If `mp sync` re-downloaded every UID whose
body blob is missing, then over a store held at its cap the loop is: sweep evicts
to get under cap, next sync re-downloads exactly what was evicted, next sweep
evicts it again — permanent evict/re-download churn, and network traffic
proportional to the overshoot on every sync. The design intent is on-demand
fetch, driven by a *user opening the message*, not by the sync refilling the
whole cache.

## Scope

1. When a message is opened (TUI preview, `mp show`, `mp read`) and its body blob
   is absent, fetch that single message from the server by UID and feed it to the
   existing same-UID re-ingest path (which already re-materialises the body and
   keeps the FTS entry in step — see
   `reingest_after_the_old_body_blob_is_evicted_leaves_one_fts_entry`).
2. Both backends: IMAP (`UID FETCH` of the one UID) and Graph (fetch by id).
3. Offline / server-unreachable degrades to today's behaviour (empty body with
   the honest sentence `src/read_cmd.rs` already prints), never an error that
   blanks the pane.
4. Attachments re-materialise on the same trigger when `mp open` / `o` needs a
   file whose blob was evicted.
5. Once this ships, drop the `>50%` `--force` guard on `mp store gc` (or relax
   it): the fat-finger accident it guards against is recoverable again.

## Acceptance criteria

- Evict a body via the sweep, open the message, and its body is back without a
  full `mp sync`.
- The re-fetch is one message, not a mailbox refill: no evict/re-download churn
  for messages the user does not open.
- Server-unreachable shows the empty-body sentence, exit code unchanged.
