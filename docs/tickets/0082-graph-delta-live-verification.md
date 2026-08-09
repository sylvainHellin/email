---
id: 0082
title: Verify the Graph delta against a live tenant (and decide on persisting Graph message ids)
type: perf
priority: later
status: open
created: 2026-08-10
---

Split out of [#0042](0042-graph-delta-sync.md), which shipped the `/messages/delta` path with its full-enumeration fallbacks but could not test it against a server: this machine has no Graph account, and the EVOQS Exchange tenant #0042 named as the first live target is not configured.

The code is fail-safe without this ticket.
Every branch that is not an exact match on a token this client minted for this folder falls back to the pre-#0042 pass, so an unverified assumption costs a full enumeration, not a missed message.
What is unproven is whether the delta ever *engages*, and how much it saves when it does.

## Scope

1. Configure the Graph account and run the #0042 acceptance criteria: a full `mp sync`, then a quick sync whose `[TIMING]` shows the delta rather than the enumeration; a change made in Outlook web propagating on the next quick sync; a `deltaLink` surviving a restart.
2. Confirm the two API assumptions #0042 took from the docs:
   - `/me/mailFolders/{id}/messages/delta?$deltatoken=latest` is accepted for mail folders and returns a `@odata.deltaLink` without enumerating. If it is not, no token is ever minted (`mint_delta_token` logs and returns `None`) and the account keeps enumerating in full, so a rejection is visible in the log and harmless; the fix would be to bootstrap from a full initial delta walk instead.
   - `Prefer: odata.maxpagesize` pages the walk, and the chain ends in a `@odata.deltaLink`. A chain that ends without one is discarded as `NoResumePoint`, which would show up as a delta that never engages twice in a row.
3. Record what the delta actually costs and saves: pages walked, wall time, and the enumeration it replaced. #0041 did this for CONDSTORE against Gmail and the numbers are what made the gate's cost arguable.

## Deferred from #0042: resolving `@removed` directly

A delta removal names the message by Graph's `id`, and the store keys a Graph row on `ingest::graph_uid(internetMessageId)`, so #0042 cannot map a removal onto a row and escalates any pass that reports one to a full enumeration.
Deletions therefore cost what they cost before #0042 and correctness is unaffected; what is lost is the saving on exactly those passes.

Resolving them directly means persisting Graph's message id per row, which is a schema column and therefore a store rebuild for every account (the no-migrator contract).
Worth doing only if the live numbers from scope 3 show that removals are frequent enough to keep the enumeration on the hot path.

## Related

- [#0042](0042-graph-delta-sync.md) shipped the delta itself.
- [#0081](0081-qresync-uidplus.md) is the IMAP twin of this leftover: a delta whose live target does not exist yet.
