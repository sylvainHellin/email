---
id: 0038
title: Read path, calendar and reconcile on the store; cold start stops walking files
type: perf
priority: next
status: done
created: 2026-07-14
---

Stage 2 of the data-access-layer redesign, rewritten 2026-07-31 for the complete nuke.
Plan: [data-access-layer](../plans/data-access-layer.md).
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), units 5 and 7.

Depends on [#0037](0037-sqlite-store-engine-skeleton.md).

This ticket is the first half of the STOP-GATE.
The pause point is the pair of this ticket and [#0050](0050-selector-contract-drafts-index.md), not this ticket alone: between them the path-taking CLI commands in `src/main.rs` still address a `.md` tree that no longer exists and drafts are not indexed, so the CLI is not usable until #0050 lands.
Restated for the greenfield build: stopping after the pair leaves a store-backed read path with mutations still optimistic and direct-to-server, which is exactly today's durability, so the stop is still viable.
The one durability item already banked is send, because the outbox ships in [#0037](0037-sqlite-store-engine-skeleton.md).

## Scope

1. `EmailEntry` loses `path` in favour of a `MessageRef` newtype, so the compiler drives the diff instead of a grep sweep across `keys.rs` and `actions.rs`.
2. `load_emails` builds the list from `messages` rows for the mailbox, sorted by `date_sort` in SQL, instead of walking and `gray_matter`-parsing files.
3. Per-mailbox counts become `SELECT COUNT(*) ... GROUP BY mailbox`, removing `count_all_emails` and its second directory walk.
4. The `build_message_id_index` startup walk is deleted outright, not replaced: identity is the `(account, mailbox, uid)` row, and cross-mailbox lookups go through the `message_id` index in SQL.
5. Lazy body: `EmailEntry.body` loads from the blob store on selection or preview, not eagerly. Audit every `EmailEntry.body` consumer first (`src/tui/ui/preview.rs`, the body-search path). Respect the `Arc`-shared `email_cache` and its generation guard; do not force a full clone per preview.
6. Calendar and reconcile move onto the store in the same flip, because both walk the `.md` tree and both break the moment ingest stops writing it: `src/tui/app/calendar_view.rs` (`load_events_for_account:97`) and `src/reconcile.rs` (`build_index`) source invites from `messages` rows and ics blobs. This is plan v2 unit 7; it depends on this ticket's units and not on [#0050](0050-selector-contract-drafts-index.md), so it can land in the same branch sequence.
7. Mutations keep today's semantics on the new substrate: flag, move, delete and archive (`src/imap_client/batch.rs:159`, `:316`, `src/graph.rs:1182`) update the store row optimistically and fire the server op directly, with the same durability as today's fire-and-forget path. [#0037](0037-sqlite-store-engine-skeleton.md) stops writing `.md`, so these ops need a local write target from this ticket onward; the durable `pending_ops` queue behind them stays [#0039](0039-pending-ops-queue.md)'s scope.

Nothing writes `.md` and nothing falls back to a file walk on a store miss: a store miss is a bug, and a fallback path would hide it.

Known issue inherited from [#0037](0037-sqlite-store-engine-skeleton.md), to be fixed with the search work here: when a message is re-ingested and its previous body blob is unreadable (evicted, or never written), `ingest_in_tx` cannot issue the FTS `'delete'` command with the old column values and leaves the stale entry in place, so that row ends up indexed twice (`src/ingest.rs`, step 4).
The duplicate resolves to a row that is still correct, so it costs a redundant hit rather than a wrong one.

## Acceptance criteria

- The golden frames captured by [#0049](0049-pre-nuke-oracle-capture.md) reproduce over the same fixture corpus. A diff is a look-and-feel regression, not a snapshot to accept.
- The portable behavioural tests (roughly 250, the audit's keep-list) are fully green, and the store-agnostic translations (roughly 15: read/unread propagation, the #0004 snapshot-cutoff race, message-id idempotence) are green against the `Store` fixture.
- The envelope dump from the new build matches the pre-nuke dump from [#0049](0049-pre-nuke-oracle-capture.md), modulo a written allow-list file in which every intended difference is one line with a reason.
- Cold start `[TIMING]` shows no tree walk and no full-file reads; `App::new` no longer touches thousands of files.
- Lazy body loads the correct content on preview; body search still finds matches.
- The calendar agenda renders the same events as the pre-nuke build over the fixture corpus, and reconcile updates attendee `PARTSTAT`s from store-backed sources.
- `cargo install --path .` clean.

## Notes

Scope item 6 landed as derive-on-read: reconciliation writes nothing at all.
Attendee `PARTSTAT`s are folded from the REQUEST/REPLY `invite.ics` blobs wherever they are displayed, and our own RSVP comes from the sent-copy REPLY that `outbox::ingest_sent_copy` files during the send itself.
The store is a droppable cache in front of the server (a schema mismatch rebuilds the file), so a persisted fold would be a second source of truth that can drift from the blobs it was computed from; deriving makes idempotence and multi-machine convergence true by construction instead of by convention.
`mp calendar rebuild` therefore reports what the fold resolves rather than rewriting anything, and its help text says so.

That also closes [TKT-0047](TKT-0047-reconcile-walks-attachment-markdown.md)'s exposure by construction: the forged-`PARTSTAT` attack needed a `.md` walk to classify a sender-controlled attachment and a frontmatter writer to persist the result, and scope item 6 deletes both.
An attachment blob is not a message row, and the invite listing selects rows joined to their own `invite.ics` blob, so there is nothing left for a crafted attachment to enter through.
The ticket stays open and is formally resolved by [#0040](0040-drop-file-layer-cutover.md), which removes the last of the file layer it was written against.

## Close-out

Done: scope items 1 to 7 are implemented and reviewed, and the last open criterion, the live envelope dump-parity check, ran on 2026-08-05.
The assistant (Gmail) and tum (IMAP) accounts pass with every difference classified against [dump-allow-list](../dump-allow-list.md), which the live evidence corrected and extended in `ab5dd1f` and `b98006f`; the 4 records missing on tum were proven deleted from the live server by exact `message-id:` search.
The proton account check was waived by the owner after its scratch store turned out to hold only a stale ~100-per-mailbox window from an earlier limited sync; no branch defect was implicated (the `-n` limit flows uncapped through `fetch_new_raw_on_session`).
Method and per-account tables: `.agents/research/live-parity-check-2026-08-05.md` (untracked).

Commits, in order: `f1adc0c` (unit A, store-backed read path, counts and dump), `58e97dd` (unit B, lazy body loading and the FTS dedup fix), `e14c5a2` (unit C, calendar and reconcile on the store), `d3e1221` (unit D, optimistic store-backed mutations) and the review-fix commit `fix(cli): boundary declines and dump allow-list entries (#0038 review fixes)`.

Deviation: the stale clap help for `mp invite` ("Path to the received invite email `.md`", `src/main.rs`) and the `website/src/pages/commands.astro` rows that still advertise `mp invite`, `mp open`, `mp save`, `mp reply` and `mp forward` as working are left as they are, owned by [#0050](0050-selector-contract-drafts-index.md), which rewrites both against the real selector syntax rather than churning an unreleased binary's docs twice.

Residual risks, accepted at review:

- A refused or offline delete is not rolled back locally: the row and the blob refcounts are gone until a later successful sync re-ingests the message from the server, where the pre-nuke build restored the local file on refusal.
  [#0039](0039-pending-ops-queue.md)'s `pending_ops` queue revisits this.
- `App::load_message_invite` lists every invite of the account and does one blob read plus one ics parse per invite on each cursor move onto an invite row, memoised per cursor position (account, message and list generation).
  Optimise only if it shows up in `[TIMING]`.

## Unblocks

- The stop-gate decision on Stages 3 to 5.
- [#0050](0050-selector-contract-drafts-index.md) (the selector resolver reads the same rows).
- [#0033](0033-view-switcher-contacts-view.md) `MailView` carve-out (store shape now settled).
