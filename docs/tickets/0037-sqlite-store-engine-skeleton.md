---
id: 0037
title: Greenfield store, blob cache, store-only ingest and the durable outbox
type: refactor
priority: next
status: open
created: 2026-07-14
---

Stage 1 of the data-access-layer redesign, rewritten 2026-07-31 for the complete nuke.
The filename slug predates the rewritten title and is kept as a stable link target.
Plan: [data-access-layer](../plans/data-access-layer.md), decisions A, B and G.
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), units 1 to 4.

Stand up the storage substrate and the ingest path that writes into it, on the greenfield branch, writing no `.md` at all.
There is no dual-write and no file fallback: the safety net is the `mp-legacy` binary and the `pre-dal-nuke` tag captured by [#0049](0049-pre-nuke-oracle-capture.md), which must be closed before this ticket opens.
The ingest slice may delete `src/sync.rs`, `src/imap_client/sync.rs` and the scanners as it replaces them.

The durable outbox is in scope here rather than in [#0039](0039-pending-ops-queue.md), because the local sent `.md` that `update_status_to_sent` writes today disappears on day one of the nuke, and best-effort APPEND is exactly the design that loses sent mail in Thunderbird, mutt and aerc.
Evidence: [sent-folder-durability-in-mail-clients](../../.agents/research/sent-folder-durability-in-mail-clients.md).

## Scope

1. New `src/store/` module: per-account `store.sqlite3` (WAL, `busy_timeout=5000`, `synchronous=NORMAL`), schema v1, version-stamped, dropped and rebuilt on version mismatch or `integrity_check` failure. Never a migrator.
2. Schema v1 is `meta`, `mailboxes`, `messages`, `message_blobs`, `blobs`, `drafts`, `outbox`, `sync_cursors`, `pending_ops`, `messages_fts`.
   `message_blobs(message_row, kind, ordinal, hash, filename, size)` is the per-message list of blob references and the source of truth for refcounting (`messages.body_blob` / `raw_blob` are a convenience denormalisation), added in unit 4a because retention evicts attachment and body blobs on separate horizons and re-ingest must release exactly the references whose content changed. `messages` carries a synthetic integer row id, `UNIQUE (account, mailbox, uid)` as the real identity and a non-unique index on `message_id`; `drafts` is keyed by `(account, id)` where `id` is the frontmatter field described in [#0050](0050-selector-contract-drafts-index.md). See the plan's schema sketch for the columns and for the four purposes the `message_id` index serves.
3. Content-addressed blob store under `<account_dir>/blobs/ab/cd/<sha256>`: write returns a hash, read by hash re-hashes to verify, dedup by existence check, refcount so a shared blob is only unlinked at zero. `sha2` is already a dependency.
4. Store-only ingest: fetch to parse to a `messages` row plus body, raw and attachment blobs. No `.md` is written anywhere on this path. Re-ingesting a UID is an UPSERT on the unique constraint; re-ingesting under a new UID after a UIDVALIDITY reset finds the prior row through the `message_id` index and keeps its thread assignment and blob refs. A missing Message-ID is synthesised as `sha256-<hex16>@local.invalid`.
5. Durable outbox: the `outbox` table holds the raw RFC822 blob, account, target mailbox, `message_id`, state (`pending_send`, `sent_pending_append`, `done`, `failed`), attempts, last error and timestamps. The blob and the row are committed before SMTP; the transition to `sent_pending_append` is committed as soon as SMTP returns 250. SMTP runs exactly once per row, and an ambiguous SMTP failure goes to `failed` for manual inspection, never to an automatic re-send. Retry drives the APPEND only, and when the previous attempt was ambiguous it first runs `UID SEARCH HEADER MESSAGE-ID` in the Sent mailbox and skips the APPEND on a hit. `APPENDUID` (UIDPLUS) is the definitive acknowledgement and its UID is stored on the row. A per-account `save_to_sent` flag defaults to `auto`, skipping the APPEND for Gmail, Graph and Proton accounts where the server already saves, and keeping it for generic IMAP. Non-`done` rows are surfaced in the TUI with a count badge, and retry resumes on startup.
6. `rusqlite = { version = "0.32", features = ["bundled"] }` added to `Cargo.toml`, plus `store_path` and `blobs_dir` helpers in `src/config.rs` beside `contacts_cache_path`.
7. New retention config schema in `src/config.rs`: the three horizons (metadata, body, attachment) and `max_disk_bytes` per account plus a global default, with the plan's documented defaults. Parsing and defaults only; eviction logic lands later. This is new surface, not an edit: `src/config.rs` has zero occurrences of `retention`, `horizon` or `max_disk_bytes` today.

Out of scope: `src/engine/` as a standalone skeleton, which is dead code until the protocol work in Stage 5; the per-account engine advisory `flock` on `<account_dir>/store.lock`, which only becomes meaningful once a second writer exists and therefore lands with the engine that takes it (see the plan's "Concurrency" section); eviction and pruning passes; server-side draft-then-submit; multi-folder Fcc.

## Acceptance criteria

- An empty account directory yields schema v1; a truncated or wrongly stamped store is dropped and rebuilt with no user-visible error; the WAL pragmas are asserted by test; the unique constraint rejects a duplicate `(account, mailbox, uid)` and accepts a duplicate `message_id`.
- Identical bytes written to the blob store twice return the same hash and touch disk once; a corrupted blob fails the read rather than returning bad bytes; a blob is unlinked only at refcount zero.
- The `parity` fixtures captured by [#0049](0049-pre-nuke-oracle-capture.md) decode to their recorded values through the store ingest path, and the iMIP fixtures classify identically. This replaces the void "byte-identical to the `.md`-derived content" criterion: there is no `.md` write left to compare against.
- Re-ingesting the same UID produces no duplicate row; re-ingesting under a new UID after a simulated UIDVALIDITY reset preserves the thread assignment through the `message_id` index.
- Kill the process between the outbox commit and SMTP: on restart the row is still `pending_send` and is sent once. Kill it between SMTP and the APPEND: on restart the APPEND completes and the Sent mailbox holds exactly one copy.
- A config with no retention section parses to the documented defaults, every field round-trips, out-of-range values are rejected clearly.
- One `cargo build --target x86_64-unknown-linux-musl` succeeds with bundled SQLite, so the release target is not a release-day surprise.
- `cargo install --path .` clean; `[TIMING]` spans added for store open and write.

## Unblocks

- [#0038](0038-read-path-to-db.md) (read path, calendar and reconcile move onto the store).
