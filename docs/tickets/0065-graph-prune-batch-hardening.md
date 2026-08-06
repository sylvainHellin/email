---
id: 0065
title: Graph prune and batch hardening (Sent-copy deletion, trim asymmetry, paging, batch ids)
type: bug
priority: now
status: open
created: 2026-08-06
---

Deferred notes 1 to 6 from the fresh-context review of [#0055](0055-graph-sync-parity.md) (commit `73ed140`).
The prune #0055 added is correct for the case it was written for and destructive for three cases it was not.
Item 1 is the priority and should ship before any Graph account sends mail; items 2 to 6 are cheap hardening on the same two functions and fit in the same session.

## Evidence

### 1. The Graph prune deletes the locally-ingested Sent copy (priority)

`src/graph.rs:763-772`.
A Graph send goes `send_durably_via` -> `target_mailbox: None` -> `src/outbox.rs:427-430` `finish_done(uid = None)` -> `ingest_sent_copy` writes a `sent` row keyed at `graph_uid(<our own Message-ID>)` (`src/outbox.rs:940-965`).
`GraphClient::send_mail` (`src/graph.rs:940-1010`) transmits JSON without a Message-ID, so Exchange stamps its own `internetMessageId`.
The local row's id therefore never appears in the Sent enumeration, lands in the vanished set, and `graph_uid` matches it exactly, so `delete_by_uid` removes the row and releases its raw MIME blob.

Usually benign, because the server's own copy is ingested earlier in the same pass and the duplicate that used to accumulate forever is cleaned up.
The failure case: if Exchange has not yet filed the item when `enumerate_folder` runs, the local copy disappears until a later sync and its raw MIME is gone permanently, because Graph never returns MIME, so "show source" for that message is dead.

This is the second half of the rationale the IMAP clamp states out loud: `src/imap_client/fetch.rs:136-140` says a Sent copy stored under `graph_uid` "is always far above any real `hi`, so it falls outside the window's range and survives".
The new doc comment at `src/graph.rs:753-762` argues only about short and empty enumerations, not about locally synthesised rows the server has never listed under that identity.

Cheapest fixes: exclude the `sent` role from the Graph prune, or skip uids whose row is younger than one sync interval.

### 2. Trim asymmetry between enumeration and ingest

`src/graph.rs:678-687` keys the enumeration map on `mid` verbatim; ingest stores `resolve_message_id` -> `mid.trim()` (`src/ingest.rs:490`).
Any padding on `internetMessageId` makes every row look vanished *and* every message look new: a delete-and-re-download loop every sync, with flags never applying because the uid is computed from the untrimmed key.
Before #0055 this was only a re-download loop; the prune makes it destructive.
One-line hardening: key on `mid.trim()`.

### 3. Paging with no stable order and no page cap

`src/graph.rs:649-696`: 25 pages of `$top=200` on a 5 000-message folder with no `$orderby`.
A concurrent arrival can shift the skiptoken window and drop a message from the map, and a dropped message is indistinguishable from a vanished one, so it gets pruned and re-downloaded.
Self-healing churn, but it is a class the IMAP clamp made impossible.
The `loop` over `next_link` also has no iteration guard.

### 4. Capped download, uncapped prune

`src/graph.rs:716`: `new.truncate(limit)` bounds the fetch, while the vanished set is the full difference.
On a quick sync (`limit = 100`) where more than 100 messages were moved to Archive at once, the inbox rows are pruned in the same run while the archive copies are still queued for later passes, so those messages hold no row anywhere (blobs unlinked) until the backlog drains.
Bounded and recoverable, but it is a window where mail is invisible.

### 5. Batch sub-request ids are not percent-encoded

`src/graph.rs:281-295`.
A direct `reqwest` GET lets `Url` encode reserved characters; Graph parses the `/$batch` sub-request `url` itself, so an id needing escaping fails *only* inside the batch.
v1.0 REST ids use the base64url alphabet plus `=`, all legal in a path segment, so the risk is low, but this is the class of bug that only shows on first real contact and no Graph live smoke has been run.
`urlencoding::encode` on the id removes it.

### 6. A permanently failing sub-request is retried every sync forever

`src/graph.rs:428-447`.
A 404/429/500 sub-response is logged and skipped, the message stays "new", and it is re-attempted on every pass.
Not silent (one `warn!` per pass) and it does not block the other 19 in the chunk, but there is no give-up state, and a 429 sub-response ignores `Retry-After` rather than pacing the rest of a large first sync.

## Scope

1. Stop the prune from deleting the locally-ingested Sent copy: exclude the `sent` role, or guard on row age against one sync interval.
   Record which and why.
2. Key the enumeration map on `mid.trim()`, matching `resolve_message_id`.
3. Give the enumeration a stable order and an iteration guard on the `next_link` loop.
4. Bound the prune by the same window the download was capped to, or defer the prune to a pass that enumerated without a cap.
5. Percent-encode batch sub-request ids.
6. Give a repeatedly failing sub-request a terminal state, and honour `Retry-After` on a 429 sub-response.

## Acceptance criteria

- A Graph send followed immediately by a sync leaves the sent row and its raw blob in place, including when the server has not yet filed the item.
- A padded `internetMessageId` does not produce a delete-and-re-download loop; a test pins the trim on both sides.
- A capped quick sync never leaves a message without a row in any mailbox.
- A sub-request that fails permanently stops being retried and is visible as failed.
