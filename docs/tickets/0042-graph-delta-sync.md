---
id: 0042
title: Graph /messages/delta with persisted deltaLink (replace poll / disk scan)
type: perf
priority: later
status: done
created: 2026-07-14
---

Parked 2026-08-06: the Graph backend is parked until it is picked up deliberately, first live target the EVOQS Exchange account; see [BACKLOG](../../BACKLOG.md).

Stage 5 (Graph) of the data-access-layer redesign. Plan: [data-access-layer](../plans/data-access-layer.md).

The single biggest Graph-account win: today Graph accounts disk-scan on every sync.
Replace the poll + scan with the delta query.
Depends on the store (Stage 2) for cursor persistence; complements [#0041](0041-persistent-conn-condstore.md).

## Scope

1. Use Graph `/messages/delta` per folder; persist `deltaLink` in `sync_cursors`.
2. Engine pull loop drives the delta query and writes changed envelopes/flags to the store, blobs for new bodies.
3. Remove the 60 s poll + every-sync disk scan for Graph accounts.

## Acceptance criteria

- Graph quick sync issues a delta query, not a full scan; `[TIMING]` confirms.
- A change made in Outlook web propagates into the store on the next sync via the delta.
- `deltaLink` survives restart and resumes incrementally.

## Shape: the delta lands in `graph.rs`'s own loop, not in `sync::engine` (decided 2026-08-10, before coding)

The scope above says "engine pull loop drives the delta query", which was written when #0059 was expected to fold both backends behind `SyncBackend`.
It did not: the parity half of #0059 is parked and `graph.rs` still runs its own orchestration (`docs/architecture.md`, "Sync backends").
So this ticket had to pick, and it picks the existing loop.

Why, in order of weight:

- The delta replaces *one step* of the Graph pass, the folder enumeration, and leaves ingest, flags, coverage and the prune untouched.
Folding Graph into `sync::engine` rewrites all of them at once, in the same change as a new resume token whose whole danger is that a mistake in it is silent.
Two risky rewrites in one commit is how #0004 happened.
- The engine's seam does not fit yet on the one axis that matters here.
`SyncBackend::fetch_targets` hands the engine a `MailboxFetch` shaped for a positional UID window and an arrival mark; the Graph pass has neither, and the delta adds a third shape (a change set that is *not* a folder listing and that nothing may diff against).
The fold wants that third shape to exist in the shared types first, which is a design question for the parity ticket rather than a side effect of this one.
- The safety gates this ticket must not disturb (#0072's deferred prune, #0074's ingest-failure bound, #0065's coverage flags) are live in `graph.rs` with tests against them.
Keeping them where they are keeps the diff reviewable as "what changed about *coverage*" rather than "what moved".

The cost is that the Graph/engine parity debt is unchanged, which is honest: it was parked before this ticket and stays parked after it.
The delta state does live where #0059 said it should conceptually, on the thing that outlives a mailbox, but for now that thing is the `sync_cursors` row rather than a `&mut self` backend field.

## What shipped (2026-08-10)

The pattern set by [#0041](0041-persistent-conn-condstore.md) for CONDSTORE, applied to Graph: a strict gate before, a resume point only a covered pass may mint, and an explicit clear.

**The token and its meaning.** `sync_cursors.deltalink` holds the `@odata.deltaLink` for `(account, mailbox)`, and it asserts exactly one thing: *at the moment this token was minted, the store held every message the folder listed*.
Everything else follows from keeping that true.
It is minted only by a pass that enumerated the whole folder, downloaded every new message it found and wrote every one of them (`may_record_delta_token`, the twin of #0041's `modseq_to_record` gate), and it is minted with `$deltatoken=latest` **before** that enumeration runs, never after, so the window between the two is replayed by the next delta rather than swallowed.

**When the delta is used.** `delta_verdict(limit, stored_token, identity_matches)`, a pure function with a matrix test.
A quick sync with a well-formed token minted against this folder takes the delta; everything else takes the full enumeration.
A full sync (`limit == usize::MAX`) always relists, which is the periodic whole-folder observation the prune and the token both lean on, and the reason drift cannot accumulate.

**Folder identity is Graph's UIDVALIDITY.** A delta token is bound to a folder *id*, not to a well-known name, so an `Archive` deleted and recreated is a different folder under the same config.
The pass reads the folder id (`$select=id`, one small GET per target) and stores its 63-bit hash in the `uidvalidity` column, which is the analogous column on purpose; a mismatch, or an id that could not be read at all, drops the token and enumerates.
Graph would answer a token for a dead folder with a 404 or 410 anyway, which the walk also discards on, but the check makes the invalidation ours rather than a server behaviour we depend on.

**Every doubt discards.** 410 (the documented expiry) and 404 are `DeltaDiscard::Expired`; any other non-success, a transport error, an unparseable page, a chain that hits `MAX_DELTA_PAGES`, and a chain that ends with neither a `nextLink` nor a `deltaLink` (no resume point, no proof it was complete) all discard the token and enumerate in the same pass.
There is one rule and no clever partial recovery: a delta that did not complete cannot be told apart from one that skipped a message.

**`@removed`: counted, escalated, never consumed by the prune.** A removal entry names the message by Graph's `id` and carries no `internetMessageId`, which is the identity the store keys a Graph row on (`ingest::graph_uid`) and which the server will not sell back for a message it has just deleted.
So the delta *cannot* map a removal onto a row.
Rather than guess, a pass whose delta reports any removal throws the change set away and enumerates the folder, and the prune keeps its existing source of truth (`known − enumerated`, `vanished_graph_uids`) with the #0065/#0072/#0074 gates on unchanged inputs.
Deletions therefore cost exactly what they cost before #0042, and nothing else does.
The rejected alternative, persisting Graph's message id per row so a removal resolves directly, is a schema column and a store rebuild for every account, and it buys deletion *latency*, not correctness; it is split out below.

**The #0074 bound applies unchanged.** The delta path feeds the same `ingest_failed` into `may_record_delta_token` that the full path feeds into its coverage flag, so while a message is still owed the token does not advance and the next pass replays; once `note_ingest_failure` gives up on it after `MAX_INGEST_ATTEMPTS`, the flag clears and the chain moves again.
A message the store will never accept cannot wedge the delta for good, which is the same failure mode #0074 fixed for the prune.

**A dry run takes the pre-#0042 pass**, because every delta branch is a store write (it either drops a token or mints one).

### Not verified live

Split into [#0082](0082-graph-delta-live-verification.md): this machine has no Graph account (`assistant` is Gmail/IMAP, `tum` is `xmail.mwn.de`/IMAP, `perso` is Proton Bridge), and the ticket's own first live target, the EVOQS Exchange tenant, is not configured.
The acceptance criteria that need a tenant (a `[TIMING]` line showing a quick sync issuing a delta rather than a scan, an Outlook-web change propagating through it, a `deltaLink` resuming across a restart) are therefore untested against a server, and two behaviours are assumed from the API docs rather than observed: that `/messages/delta?$deltatoken=latest` is accepted for mail folders, and that `Prefer: odata.maxpagesize` pages the walk.
Both are fail-safe if wrong: a refused mint means no token is ever stored and the account keeps enumerating in full with a warning per pass, and a paging failure discards the token and enumerates.

## Related

- Graph sync backend for calendar / server-side RSVP is a separate track ([#0036](0036-graph-sync-backend.md), blocked on [#0035](0035-graph-admin-approval.md)).
