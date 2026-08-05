---
id: data-access-layer
title: "Data-access-layer redesign: server-as-truth SQLite mirror + local-only drafts"
status: DECIDED (2026-07-14), amended 2026-07-31 for the complete-nuke greenfield rebuild
supersedes: ".agents/research/2026-07-12-architecture-rethink-decision-doc.md (Option choice), .agents/research/2026-07-12-perf-sync-improvement-plan.md (Phases 0-2)"
next_free_ticket: 0051
---

# Data-access-layer redesign

The paradigm question is settled.
mailypoppins stops treating local `.md` files as the source of truth.
The server is ground truth for received mail; a local SQLite mirror plus a content-addressed blob store is the client's fast, disposable copy; drafts are the only thing whose truth is local.
This is Option B from the decision doc, with three owner amendments recorded below.

> Do not reopen the paradigm question.
> The decision doc (`.agents/research/2026-07-12-architecture-rethink-decision-doc.md`) is the argument; this doc is the plan.
> If a future session wants the reasoning, read the decision doc; if it wants to build, read this.

## The decision, and the amendments to Option B

Confirmed 2026-07-14 by the owner:

1. Server is ground truth for received mail; the client mirrors it locally and reconciles.
2. Drafts are local-only files and never sync to the server.
   This is simpler than the decision doc's Option C (which synced drafts through `pending_ops` and needed a watched-drafts shim).
   Trade-off accepted: drafts do not converge across the two machines.
3. Received mail is read-only.
   Editing in `$EDITOR` is for drafts only.
   This settles the decision doc's Decision 3 as option (i), the biggest behavioural change and the one the owner explicitly accepted.
4. No Obsidian / live `.md` tree / `grep` affordance.
   Terminal `$EDITOR` access to a message is all that is required.
   This settles Decision 2 as "drop entirely"; full-text search is served by FTS5, not by `grep`.
5. Bodies and attachments live in a content-addressed blob store on disk, not inside SQLite.
6. A persistent authenticated IMAP connection is approved, replacing the "one session per operation" invariant (Stage 5).

What survives from the founding product, unchanged:

- Read mail in the terminal TUI.
- Compose and edit drafts in `$EDITOR` (Neovim / Helix), the original raison d'etre.
- Agents (Claude / Robin) create and modify drafts as plain files the owner reviews and sends.
- Send from the terminal after approval.

### Decisions settled 2026-07-31

Recorded after owner review of the foundation plan v2 ([2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md)) and the sent-durability research ([sent-folder-durability-in-mail-clients](../../.agents/research/sent-folder-durability-in-mail-clients.md)).
These are settled and do not reopen.

A. Complete nuke, greenfield on a long-lived branch with delete-as-you-go.
   [#0037](../tickets/0037-sqlite-store-engine-skeleton.md) and [#0038](../tickets/0038-read-path-to-db.md) may delete `src/sync.rs`, `src/imap_client/sync.rs` and the scanners as they go; no dead code is carried forward.
   The safety net is the `mp-legacy` binary plus the `pre-dal-nuke` tag, never a dual-write layer.
B. Sent durability ships with the greenfield ingest slice ([#0037](../tickets/0037-sqlite-store-engine-skeleton.md)) as a durable outbox, per the research recommendation.
   The `outbox` table holds the raw RFC822 blob with states `pending_send`, `sent_pending_append`, `done` and `failed`, and the blob and row are committed before SMTP.
   SMTP is attempted exactly once per row: an ambiguous SMTP failure goes to `failed` for manual inspection and never to an automatic re-send.
   Retry drives the APPEND only, guarded by a `UID SEARCH HEADER MESSAGE-ID` check in the Sent mailbox when the previous attempt was ambiguous, with `APPENDUID` as the definitive acknowledgement.
   A per-account `save_to_sent` flag defaults to `auto`, which skips the APPEND on Gmail, Graph and Proton where the server already saves, and keeps it for generic IMAP.
   Non-`done` rows are surfaced in the TUI with a count badge, and retry resumes on startup.
C. Draft identity is an `id:` frontmatter field written by `mp new`, so a rename does not change the selector.
   The `drafts` table is keyed by `(account, id)`, agents and automation are expected to preserve the field, and a draft file without one is assigned an id on the first index refresh.
D. Golden frames stay lean: the mail view (sidebar, list, preview) and the calendar at 120x40, plus the help overlay.
   The style-run legend is captured only where style carries meaning (unread bold, cursor row, selection), and there is no multi-size sweep, because a cosmetic snapshot catches no regression worth the churn.
E. `mp dump-mailbox --json` may land on `main` before the freeze, since it is the only way the envelope-dump oracle exists at all.
F. TKT-0047 is parked with an explicit accepted-risk note and marked resolved-by [#0040](../tickets/0040-drop-file-layer-cutover.md); no code is spent on the module the nuke deletes.
G. `messages` carries a synthetic integer row id with `UNIQUE (account, mailbox, uid)` as the real identity and a non-unique index on `message_id`; ingest dedup is the UPSERT on that unique constraint, and a missing Message-ID is synthesised as `sha256-<hex16>@local.invalid`.
H. One CLI contract, `mp://<account>/<mailbox>/<key>`, committed on landing: no path inputs anywhere in the CLI, `mp path` and `mp edit` are the only filesystem edge, no command ever dual-accepts a path and a selector, and TKT-0045 is resolved by the drafts index rather than fixed in the current build.

## Why files-as-truth is being retired (verified against the tree)

The cost, measured against the current code, not asserted:

- Cold start streams every byte of the tree.
  `build_message_id_index` (`src/tui/app/types.rs:475`) walks 1327 files / 308 MB on the homeserver (thousands more on the Mac) at startup; `load_emails` (`types.rs:88`) full-parses a mailbox with `gray_matter`; `count_all_emails` (`types.rs:1176`) walks every directory a second time.
- Quick sync is slow because it does the maximum work every time.
  Per mailbox it opens a fresh TCP + TLS + LOGIN (`open_imap_session`, `src/imap_client/mod.rs:178`), runs `UID SEARCH ALL`, then a full-window pass-1 header + FLAGS fetch that is deliberately never shrunk because flags have no delta channel (`src/imap_client/fetch.rs:171`, the #0004 constraint).
  Multiply by mailboxes across four accounts.
  Apple Mail is smooth because it holds a persistent connection, uses IDLE push and CONDSTORE/QRESYNC deltas ("what changed since cursor N", one round trip, empty when nothing changed), and reads a local index instead of parsing files.
- Reconciliation is heuristic and ambiguous.
  `needs_reconciliation` (`src/imap_client/sync.rs:42`) guesses move/delete from EXISTS/UIDNEXT deltas, and out-of-band file edits collide with server truth with no principled winner.

Under the new model none of these exist: the read path is a `SELECT`, sync is a cursor delta, and the only writers to state are the sync engine and the user's own actions, which removes the corruption surface the owner flagged.

## Target architecture

One engine, one store, one writer per account.
This is the Mailspring / JMAP shape (research doc `.agents/workflow/2026-07-12-db-first-redesign/.../research-local-first-email.md`, findings 1, 5, 8), adapted to this codebase.

### Storage layout (per account)

```
<account_dir>/
  store.sqlite3        WAL, single writer: envelopes, flags, threading, mailboxes,
                       sync_cursors, pending_ops, outbox, the drafts index,
                       + FTS5 over subject/from/body-text
  store.sqlite3-wal
  store.sqlite3-shm
  blobs/
    ab/cd/abcd1234...  content-addressed raw RFC822 / decoded body / each attachment,
                       filename = SHA-256 of the bytes
  drafts/              plain .md drafts, local-only, the $EDITOR + agent surface
```

`store.sqlite3` sits beside the existing `mailbox-states.json` and `contacts-cache.json`, matching the current per-account cache layout.

Content-addressed blob store: each raw body / HTML / attachment is a file named by the hash of its own bytes, and the DB row keeps only a `blob_ref` hash.
This gives automatic dedup (a forwarded attachment is stored once), integrity checks (re-hash to verify), and immutability (content never changes under a key), and it keeps SQLite small and fast by never storing large blobs inline.
The existing stable-hardlink attachment mirror (#0006) maps directly onto this store.

### Retention tiers and disk budget (configurable)

Because the server is truth, local storage is a cache the client is free to shrink and re-fill on demand.
Anything evicted locally is always re-fetchable from the server, so eviction is lossless, not deletion.
This is what makes fine-grained retention control safe, and it is a first-class config surface, not an afterthought.

Three independent retention horizons, coarsest-to-finest, each a config field per account (and a global default):

1. Metadata horizon: how far back to keep envelope rows (from/to/cc/subject/date/flags/thread). Cheap; default is keep-all so the list and search always render for the full history.
2. Body horizon: how far back to keep full message bodies (the body blob). Older bodies are evicted; the envelope row stays with its `blob_ref` marked evicted, and the body is re-fetched from the server when the user opens the message.
3. Attachment horizon: how far back to keep attachment blobs. Independent of and typically shorter than the body horizon, since attachments dominate disk.

On top of the horizons, a global disk budget: a per-account (and total) `max_disk_bytes`.
When usage exceeds it, a pruning pass evicts blobs oldest-first until under a low-water mark.
Pruning runs in cycles with hysteresis (a high-water trigger and a lower stop target), not on every message save, so a busy sync does not thrash the evictor.

Contradiction and precedence: the disk budget is a hard cap that overrides the horizons.
The horizons express intent ("I want bodies for a year"); the budget enforces reality ("but never past 5 GB").
Within a pruning pass the order of application is the precedence, as the owner proposed: evict attachment blobs first, then body blobs, then (only if still over budget) trim metadata beyond the metadata horizon.
Envelope rows are evicted last and rarely, because they are what lets an evicted message still appear in the list and be re-fetched.

Implementation notes:

- Eviction sets the row's blob state to evicted and unlinks the blob file (respecting dedup: only unlink when refcount hits zero); it never touches the server.
- Re-fetch on open is a normal engine pull for one message by UID; the body arrives, the blob is rewritten, the row flips back to present.
- The blob store needs a refcount or a periodic mark-and-sweep GC so a shared attachment is not unlinked while another message still references it.
  Shipped as the `blobs(hash, size, refcount)` table inside the per-account `store.sqlite3`, so a reference is taken in the same transaction as the row that carries the hash.
- Config lives in the existing config file; defaults must be sane (keep-all metadata, generous body/attachment horizons, a conservative disk cap) so a user who sets nothing still gets correct behaviour.

Config shape as implemented in `src/config.rs` (#0037 unit 3), parsing and defaults only:

```toml
[retention]                     # global defaults, all fields optional
metadata_horizon_days = 0       # 0 means keep all
body_horizon_days = 365
attachment_horizon_days = 90
max_disk_bytes = 5000000000

[accounts.retention]            # per-account override, field by field
attachment_horizon_days = 30
```

Horizons are integer days and `0` means keep everything; the disk budget is raw bytes.
An account value wins field by field, an unset account field falls back to the global `[retention]` table, and an unset global field falls back to the default.
The defaults are metadata `0` (keep all envelope rows), body `365` days, attachments `90` days and `max_disk_bytes` 5 GB.
Validation runs at config load: horizons must be between `0` and `36500` days (100 years), `max_disk_bytes` between `10000000` (10 MB) and `1000000000000` (1 TB), and a violation names the field, the value and the allowed range.
An account whose retention section is absent entirely resolves to exactly those defaults.

### SQLite schema (v1 sketch, drop-and-rebuild on version mismatch, never a migrator)

```
meta(key PRIMARY KEY, value)                         -- schema_version, app_version
mailboxes(account, name PRIMARY KEY, uidvalidity, uidnext, exists_count, unread_count)
messages(
  id INTEGER PRIMARY KEY, account, mailbox, uid,
  message_id, from_, to_, cc, subject, date_sort, date_display,
  flags, in_reply_to, references_, thread_id,
  snippet, has_attachments, body_blob, raw_blob, size, mtime,
  UNIQUE (account, mailbox, uid)
)
CREATE INDEX messages_message_id ON messages(message_id)   -- non-unique
drafts(account, id PRIMARY KEY, slug, path, mtime, size, status, to_, cc, subject, date, snippet)
outbox(id INTEGER PRIMARY KEY, account, target_mailbox, message_id, raw_blob,
       state, attempts, last_error, created, updated)
sync_cursors(mailbox PRIMARY KEY, uidvalidity, highest_modseq, deltalink)
pending_ops(id PRIMARY KEY, kind, target_message_id, payload, state, attempts, last_error, created)
messages_fts USING fts5(subject, from_, body_text, content='messages')
```

The identity of a received message is `(account, mailbox, uid)`, not its Message-ID, and the row id is synthetic so a move or a UIDVALIDITY reset does not invalidate foreign references.
Ingest dedup is the UPSERT on that unique constraint, so the `message_id` index is never on the hot ingest path.
It exists for four things: resolving `In-Reply-To` and `References` to a parent row so `thread_id` can be assigned at ingest; idempotent re-ingest after cursor loss, so local-only state survives when the same message reappears under a new UID; cross-mailbox copy detection, so an already-archived message is a no-op rather than a second copy; and stale selector resolution, so a selector that no longer matches a `(mailbox, uid)` row is answered with the message's new location.
Because Message-IDs are sender-controlled and can be absent, ingest synthesises `sha256-<hex16>@local.invalid` from the raw bytes when the header is missing, which keeps the column non-null and the selector total.

`drafts` is a derived index over `<account_dir>/drafts/`, keyed by the `id:` frontmatter field that `mp new` writes; the file stays truth and the table is refreshed on engine start, after any `mp` command that writes a draft, and by a one-second mtime scan of that single directory.
`outbox` is the durable sent path described in decision B.

`rusqlite = { features = ["bundled"] }` is a new static dependency (bundled SQLite + FTS5, no system libsqlite), validated by meli in the research.
Bundled SQLite compiles C, so the first store commit runs one `cargo build --target x86_64-unknown-linux-musl` to confirm the release target still builds before that becomes a release-day surprise.

### The engine

- One long-lived task per account owns the store and the sync loop.
  The TUI reads the store and enqueues intents; it never touches IMAP or the schema directly (preserves the TUI-never-implements-email-logic invariant).
- `pending_ops` is a durable two-phase queue.
  Every flag, move and delete mutation applies to the local store immediately, enqueues a remote op, and the engine drains it with retry and backoff, marking done or failed.
  This is the optimistic-mutation model the TUI already has via `Action` / `BgResult`, made durable instead of fire-and-forget.
  Send is not one of these kinds: it has its own `outbox` table with a stricter state machine, because SMTP is not retryable the way a flag change is.
- Per-folder sync cursors drive incremental pull: QRESYNC where advertised, else CONDSTORE flag-delta with a persisted HIGHESTMODSEQ, else today's EXISTS/UIDNEXT heuristic; Graph accounts use `/messages/delta` with a persisted `deltaLink`.
- The engine emits `BgResult` variants over the existing `mpsc` channel; `App::update` stays a pure state machine and only the source of `BgResult` changes.
  The existing TEA architecture is the asset that makes this a clean fit.

### Concurrency: running the TUI and the CLI at the same time

The single-writer discipline is per-account, and it must hold even when two processes are live (a TUI in one terminal, `mp` in another).
SQLite WAL already allows many concurrent readers plus one writer, with writes serialized by the database lock and `busy_timeout` smoothing brief contention.
That covers the store safely, but two processes each running their own sync engine (each holding IMAP connections and each writing) would double the server load and race on `pending_ops`.
The rule: at most one engine per account across all processes.

The mechanism is a non-blocking exclusive advisory lock (`flock`-style) on `<account_dir>/store.lock`, taken by whichever process runs the engine and released when that process exits or dies, which is what makes the takeover below automatic.
The lock lands with the engine rather than with the store skeleton, because it is only meaningful once a second writer can exist, so [#0037](../tickets/0037-sqlite-store-engine-skeleton.md) defers both.
SQLite's own locking is not the mechanism: its locks are scoped to a transaction, so they can serialise writes but cannot express "one engine per account for the lifetime of the process".
The protocol around the lock:

- The first process to open an account acquires the engine lock and runs the sync engine (IDLE, pulls, draining `pending_ops`).
- A second process (the CLI while the TUI is open) opens the store read-only for queries and, for mutations, writes a `pending_ops` row and lets the lock-holding engine drain it. It does not open its own IMAP connection.
- If the lock-holder exits, the next process to need the engine acquires the lock and takes over.

Consequence: `mp search`, `mp` read commands, and even enqueuing a send all work fine while the TUI is open; they read the shared store and hand mutations to the one running engine.
No clashes, and no duplicate server sessions.
This is cleaner than today, where each `mp` invocation opens its own IMAP session independently.
A later refinement (out of scope for the first cut) is a proper background daemon that owns the engine and to which both TUI and CLI are thin clients; the advisory-lock model is the lightweight version of the same idea and is enough to start.

### What survives in the code (~40% intact)

The RFC822 parser and MIME extraction (`src/parse.rs`), `markdown_to_html` and per-recipient SMTP send (`src/send.rs`), OAuth2 / XOAUTH2 and the encrypted secrets store, the Graph REST client (`src/graph.rs`), config loading, contacts mining (`src/contacts/`), IMAP session open and capability handling, `TimingSpan`, and the entire TEA / `Action` / `BgResult` scaffolding.
What disappears (~25%) is the files-as-truth machinery: the two full-file scanners (`parse.rs:756`, `:989`), `deduplicate_mailbox`, `count_all_emails`, the `MessageIdIndex` HashMap, and the reconciliation heuristics.

Two tree-walking modules the original split missed, both of which land in [#0038](../tickets/0038-read-path-to-db.md) rather than in the cutover:

- `src/tui/app/calendar_view.rs` (975 lines, `load_events_for_account:97`) reads iMIP events straight out of the mailbox directories, so the agenda is broken from the moment ingest stops writing `.md`.
- `src/reconcile.rs` (491 lines, `build_index`) walks the account root to reconcile attendee replies, which is also where TKT-0047 lives.

Both source invites from `messages` rows and ics blobs after the flip; neither is a Stage-4 afterthought.

## Staged transition, with a stop-gate after Stage 2b

No big-bang and no dual-write.
The lever is the no-migrations-before-v1.0 rule: the cutover in Stage 4 is not a migration, it is wipe-the-local-tree-and-resync-from-the-server, with a one-time import of drafts.

| Stage | Ticket | Ships | De-risks | Effort |
|---|---|---|---|---|
| 0 Pre-nuke oracle capture | [#0049](../tickets/0049-pre-nuke-oracle-capture.md) | Golden frames, gap-list fixtures, `mp dump-mailbox --json` envelope dumps, then the `pre-dal-nuke` tag and the `mp-legacy` binary. | The absence of an oracle. Nothing greenfield opens until this closes. | M |
| 1 Greenfield store, ingest and outbox | [#0037](../tickets/0037-sqlite-store-engine-skeleton.md) | `src/store/` (schema v1, WAL, blob store, retention config) plus store-only ingest and the durable outbox. No `.md` is written on the ingest path; drafts stay file-truth. | Schema, WAL single-writer discipline, blob store, and sent durability, which the nuke would otherwise regress. | L |
| 2 Read path on the store | [#0038](../tickets/0038-read-path-to-db.md) | `load_emails`, list render, counts, search, the calendar agenda and reconcile all read the store; cold start stops walking files. | List-render perf, envelope correctness and look-and-feel parity against the golden frames. | L-XL |
| 2b Selector contract and drafts index | [#0050](../tickets/0050-selector-contract-drafts-index.md) | `mp://<account>/<mailbox>/<key>` everywhere, the `drafts` table and the CLI rewrite, in one commit. | The CLI contract, committed once rather than migrated. Subsumes TKT-0045. STOP-GATE, after this ticket and not after Stage 2 alone. | M-L |
| 3 Remaining mutations to `pending_ops` | [#0039](../tickets/0039-pending-ops-queue.md) | Archive/delete/move/mark-read become durable queue rows; engine drains with retry/backoff. Send is already durable via the outbox. | The op-queue, the genuinely new core infra and its new bug class. | L |
| 4 Legacy decommission | [#0040](../tickets/0040-drop-file-layer-cutover.md) | Retire the legacy `.md` tree and `mp-legacy`, one-time draft import assigning `id:` frontmatter, TKT-0047 closed. | The end of the transition period, not a code deletion (the greenfield build never had the file layer). | S-M |
| 5 Persistent conn + protocol + FTS | [#0041](../tickets/0041-persistent-conn-condstore.md), [#0042](../tickets/0042-graph-delta-sync.md), [#0043](../tickets/0043-fts5-search.md) | Persistent authenticated connection, CONDSTORE/QRESYNC flag-delta, Graph `/messages/delta`, FTS5 search. | Sync smoothness and full-text search. | M-L |

The stop-gate is still real, and it is what the greenfield ordering buys, but it sits after the Stage 2 and Stage 2b pair rather than after Stage 2 alone.
Stage 2 moves the read path onto the store; Stage 2b is what puts the path-taking CLI commands and the drafts index back on a tree that no longer holds a `.md` mailstore, so the pause point is the pair and the CLI is not usable between them.
After Stage 2b the owner has the perf win (cold start and quick-sync render both stop touching thousands of files) with mutations still optimistic and direct-to-server: Stage 2 updates the store row and fires the server op directly, which is exactly today's semantics and durability, and the durable queue behind those ops is Stage 3's scope.
Pausing there is viable; the only thing already promoted out of Stage 3 is send, because the outbox ships in Stage 1.

### The complete nuke (decided 2026-07-31)

The redesign is a greenfield rebuild on a long-lived branch, with delete-as-you-go: Stages 1 and 2 remove `src/sync.rs`, `src/imap_client/sync.rs` and the scanners as they replace them, and no dead code is carried forward for a fallback that will never be used.
The safety net is not a code path, it is the preserved binary and the tagged tree.

Both were captured in Stage 0 ([#0049](../tickets/0049-pre-nuke-oracle-capture.md)), together with the envelope oracles, before a line of greenfield code existed:

1. `~/.local/bin/mp-legacy`, copied from the cargo-installed `mp` built at the frozen commit; it keeps reading the existing `.md` tree and works forever against its own data, and the redesign becomes the new `mp`.
2. The tag `pre-dal-nuke`, on the last files-as-truth commit, so the frozen reference is addressable rather than remembered.
3. The real-account envelope dumps from `mp dump-mailbox --json`, in the git-ignored `dumps/` directory as `dumps/pre-nuke-<account>.ndjson` (assistant 312 records, tum 627, perso 450), kept out of git because they carry real mail metadata.

Recorded here as the facts a future session needs: the tag is `pre-dal-nuke`, the preserved binary is `~/.local/bin/mp-legacy`, and the envelope oracles are local-only under `dumps/`.

The one caution holds without exception: `mp-legacy` (files-as-truth) and the rewritten `mp` (store + blobs) must never point at the same account data directory, or the new version's wipe-and-resync will delete the `.md` files the old one treats as truth.
Use separate data dirs, or do not run both against one account.

The alternative of keeping two crates compiling side by side was rejected: it is the dual-write cost wearing a different hat, and it pays for a fallback the branch already provides for free.

## Decision points already settled (do not re-ask)

- Storage backend: SQLite, not JSON, not NoSQL. Straight to SQLite because the envelope store (Stage 2) and FTS5 (Stage 5) need it and a JSON mirror becomes a full-rewrite-per-save liability.
- Bodies: content-addressed blob files, not inline in SQLite.
- Received mail: read-only. Drafts only in `$EDITOR`.
- Drafts: local-only, never synced.
- Obsidian / `grep`: dropped; FTS5 replaces search. Note the in-app search today is server-side IMAP SEARCH (`f` in the TUI, `mp search` in the CLI), not a local `grep`; FTS5 is a new local capability that also makes search work offline and without a round trip. There is no existing local full-text index to reuse.
- Persistent connection: approved; the "one session per operation" invariant in `docs/architecture.md:23` gets rewritten in Stage 5.

## Resolved unknowns (2026-07-14)

Backing research: the sync mechanics explainer [imap-sync-internals-explainer](../../.agents/research/2026-07-14-imap-sync-internals-explainer.md) and the [server-capability-matrix](../../.agents/research/2026-07-14-server-capability-matrix.md).

- async-imap 0.11.2 surface (was unverified): typed `select_condstore()` exists and the parser (imap-proto 0.16.7) already decodes `HIGHESTMODSEQ`, per-message `MODSEQ`, and `VANISHED (EARLIER)` into typed responses. `run_command` / `run_command_and_check_ok` / `read_response` are public on `Session`. So CONDSTORE is largely typed (CHANGEDSINCE goes in the fetch modifier string), and QRESYNC/ENABLE are feasible via the raw escape hatch with parser support already present. No blocker; a small spike still confirms the CHANGEDSINCE fetch-string form.
- Server capabilities (was unverified): Proton Bridge (Gluon) does NOT support CONDSTORE or QRESYNC (only IDLE + UIDPLUS + MOVE), so the owner's daily Proton account stays on the EXISTS/UIDNEXT heuristic + IDLE tier and the #0004 full-window flag fetch remains correct there. Gmail supports CONDSTORE + UIDPLUS + IDLE but not QRESYNC. Dovecot (likely tum) supports the full QRESYNC ladder. Outlook syncs via Graph delta, not IMAP. Gate each mechanism on live CAPABILITY.
- Test targets (was: no harness): validate live against the real accounts rather than blocking on a mock. Robin's Gmail exercises CONDSTORE + UIDPLUS; tum/Dovecot exercises QRESYNC; Proton Bridge exercises the heuristic + IDLE + UIDPLUS path. A mock harness can come later; live-account validation unblocks Stages 3 and 5 now.

## Still open, to resolve during implementation

- The suite is 656 tests (589 lib, 67 integration: 65 passing and 2 ignored) with 198 `tempdir()` usages, which is the file-layer blast radius, concentrated in `src/draft.rs` (34), `tests/draft_integration.rs` (28), `src/sync.rs` (23) and `src/parse.rs` (19).
  The test audit classified the tests the rewrite has to decide on, and put them in three buckets: roughly 250 port unchanged because they compile against the library API rather than storage (iMIP, the parser and sanitiser, drafts, TUI key and filter invariants, contacts), roughly 15 are translated to a `Store` fixture (read/unread propagation, the #0004 snapshot-cutoff race, message-id idempotence), and roughly 130 file-layer tests are discarded rather than ported.
  The three buckets are not a partition of the full suite; the remainder is trivial or out of scope for the rewrite and needs no decision.
  Budget the translation and the discard as first-order costs in Stage 1 and Stage 2, not a rounding error.
- Confirm tum is Dovecot with a live CAPABILITY probe before relying on its QRESYNC (the probe recipe is in the capability matrix doc).
- Graph deltaLink expiry handling: treat an invalidated deltaLink like a UIDVALIDITY reset (full resync of that folder).

## Relationship to #0033 (MailView carve-out)

#0033's `MailView` state carve-out is blocked on this plan because it reshapes the same `App` mail-data fields (`emails`, `mailboxes`, `mailbox_counts`, `MessageIdIndex`, `load_emails` / `count_all_emails`) that Stage 2 rewrites.
Settle the store shape through Stage 2, then carve `MailView` once against the final shape.
Do not carve before Stage 2 lands.

## Verified code anchors the implementation will touch

- Read path: `src/tui/app/types.rs:88` (`load_emails`), `:475` (`build_message_id_index`), `:1176` (`count_all_emails`).
- Scanners to delete: `src/parse.rs:756` (`scan_mailbox_message_ids`), `:989` (`deduplicate_mailbox`).
- Reconciliation to replace: `src/imap_client/sync.rs:42` (`needs_reconciliation`), `:30` (`MailboxState`), `:86` (`mailbox-states.json` cache, the sibling-cache precedent).
- Fetch to reshape into the engine: `src/imap_client/fetch.rs:171` (`fetch_new_emails_on_session`), pass-1 headers `:213`.
- Session open to make persistent: `src/imap_client/mod.rs:178` (`open_imap_session`).
- Invariant to rewrite in Stage 5: `docs/architecture.md:23` ("one session per operation").
- TEA seam that stays: `Action` / `BgResult` in `src/tui/`, `App::update`.
