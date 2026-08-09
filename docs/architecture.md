# Architecture

How mailypoppins is put together.
Read this before non-trivial changes.

## Project invariants

- The server is truth, the store is a cache.
Received mail lives in a per-account SQLite file plus a content-addressed blob store, and both are disposable: a schema mismatch, a failed integrity check or an unreadable file is answered by deleting the store and letting the next sync refill it.
Nothing in the store may be the only copy of anything the user typed.
- Drafts are the only local truth.
They are Markdown files with YAML frontmatter under `<account_dir>/drafts/`, written by `mp new` and by `$EDITOR`, and the `drafts` table is a derived index over them.
- Received mail is read-only locally.
The client never edits a message body; it changes flags and mailbox membership, and the server is told immediately.
- No migration paths until v1.0.
When changing data formats, secret storage or wire protocols, drop the old code and prompt the user to reconfigure.
Do not write v1 to v2 migrators.
- The TUI implements no email logic.
No SMTP, IMAP, MIME or Graph REST code belongs in `tui/app/` or `tui/ui/`; the TUI layering section below states what those two layers do and do not touch today.
- Windows is targeted via WSL only.
No native-Windows code paths (registry, Credential Manager).

## Crate shape

Single crate, library plus binary.
All logic lives in `src/lib.rs` modules so the TUI can call them directly without subprocess spawning.
Config types derive `Clone` so they can be moved into background threads.

The installed binary is `mp` (`cargo install --path .`).
The Cargo package and library are `mailypoppins` (#0022), so imports read `use mailypoppins::...` and `insta` snapshot files are prefixed `mailypoppins__`.
The user-facing name and version string is `mailypoppins X.Y.Z`, set via clap `#[command(name = "mailypoppins")]` and `#[command(version)]` in `src/main.rs`; the Homebrew formula test asserts against that string.
The one place the old spelling survives is the keyring service fallback below.

## The store

One `store.sqlite3` per account, in the account directory, opened in WAL mode with a 5 s busy timeout and `synchronous = NORMAL` (`src/store/mod.rs`).

The drop-and-rebuild contract is the reason there is no migrator.
`Store::open` rebuilds the file from scratch when the stamped schema version is not the current one, when a required table is missing, when `PRAGMA integrity_check` fails, or when the file does not open as a database at all.
None of those is a user-visible error: the store holds no truth, so the answer is a log line and a file built from scratch.
The integrity check walks the whole file, so it runs once per file per process rather than on every open.

One table is not a cache and does not go with the file: `outbox` (#0066, `src/store/rebuild.rs`).
Before the old file is deleted its unfinished rows (`pending_send`, `sent_pending_append`, `failed`) are read back defensively, by column name, so an outbox of an older shape still comes across, and are written into the new file with a reference on the raw RFC822 blob each one points at.
`done` rows owe nothing and stay behind.
A row that cannot be carried, because its bytes are gone from the blob store or its columns are unreadable, is named in a `store-rebuild-<timestamp>.txt` note written next to the store; nothing about a submitted message is discarded silently.
The same pass then sweeps the blob tree, deleting every file the rebuilt store holds no refcount row for, so a rebuild cannot leave the blob directory full of orphans that nothing reclaims.

Schema v6 lives in `src/store/schema.rs`, which carries the identity notes in full; the short version:

- `messages` is one row per message per mailbox, with a synthetic `id` and `UNIQUE (account, mailbox, uid)` as the real identity.
The same message in two mailboxes is two rows.
Its `flags` column holds the IMAP flag string, which is where the second status axis lives (#TKT-0051): `\Seen`, `\Answered` and the `$Forwarded` keyword, parsed into `types::MessageFlags`.
Three independent bits rather than one state, and no schema change, because the column was already there and every sync pass restates it for the whole window.
The `messages_message_id` index is deliberately non-unique: it serves threading, idempotent re-ingest, cross-mailbox copy detection and selector resolution.
- `blobs` is the refcount index for the content-addressed blob store, and `message_blobs` is the per-message list of `body`, `html`, `raw` and `attachment` references.
Refcounts live in the database so a reference can be taken in the same transaction as the row that carries the hash.
- `messages_fts` is a contentless FTS5 index (`content=''`, `contentless_delete=1`) over subject, from and body text.
Only `rowid`-returning `MATCH` queries work; there is nothing to rebuild from, and nothing needs to be, because a store that loses its index is dropped.
- `sync_cursors` is keyed by `(account, mailbox)` and keeps `last_uid` (where the IMAP pull resumes) apart from `highest_modseq` (a CONDSTORE sequence, NULL until #0041) and `deltalink` (Graph, NULL until #0042).
The two were one column until #0054, which stored a UID where a modseq was read back.
`arrival_mark` (v5, #0072) is the one column here a later pass reads back: the UID above which the mailbox still owes the store a message the server lists, which keeps the prune gate shut until a pass reaches through it.
A message the pass downloaded and then failed to write pulls that mark under itself (v6, #0074), because a message not written is as absent as one never fetched.
- `ingest_failures` (v6, #0074) counts those failures per `(account, mailbox, uid)` and bounds them: after `ingest::MAX_INGEST_ATTEMPTS` passes the UID is given up on loudly and stops holding the mark down, so a message the store rejects deterministically cannot suspend the prune for the whole account for good.
A successful ingest deletes the row, so transient failures never accumulate towards the bound.
- `outbox` carries the durable send state machine described below.
- `drafts` is the derived index over the drafts directory.
- `pending_ops` carries the durable mutation queue (#0039): one row per owed server op, with `kind`, the `messages` row id in `target_message_id`, the full `ServerOp` plus its rollback in the JSON `payload`, and a `queued` / `failed` state. `src/pending_ops.rs` owns it, the mutation twin of `outbox`. Like `outbox` its live paths are not the schema's concern, but unlike `outbox` it is a plain cache table: a lost queue row loses a flag change or delays a move, never a message, so it is dropped and rebuilt with the file.
- Every table carries `account`, although one file holds one account.
The redundancy keeps a future shared database a schema change rather than a rewrite of every query.

The blob store (`src/store/blobs.rs`) is `<account_dir>/blobs/ab/cd/<sha256>`: every raw message, decoded body and attachment is a file named by the hex SHA-256 of its own bytes.
The name is the content, which buys dedup, verification (a read re-hashes and refuses bytes that no longer match their name) and immutability.
Blob files are written before the transaction that references them, never inside it: an unreferenced blob is a harmless orphan a sweep reclaims, while a row pointing at a missing blob is a hole in the read path.

## Data flow

### Receive

A sync backend hands raw messages to `src/ingest.rs`, the only writer on the receive path.
One transaction per message writes the `messages` row, its blob references and its FTS entry, so a crash leaves whole messages behind and never half of one.
Re-ingesting a UID is an UPSERT that keeps the row `id`, its thread assignment and the blob references whose content did not change.
After a UIDVALIDITY reset the row is found by Message-ID and rebound to the new UID in place.
A message with no `Message-ID` header gets a deterministic `sha256-<hex16>@local.invalid` synthetic id.

### Read

Everything the TUI, `mp dump-mailbox` and the contact index show comes from `src/store/read.rs` and `src/store/drafts.rs`.
There is no directory-walk fallback: nothing writes `.md` for received mail, so a missing row is a bug in ingest and a walk that quietly produced the message anyway would hide it.
Attachments are blobs, so anything that needs a file materialises them: `mp open` and the TUI's `o` into a private temp directory keyed by the row, a forward draft into `<account_dir>/attachments/<message-id>/` so the draft keeps resolving them after the source row is archived or evicted.

### Mutate

A flag, move, archive or delete is one local write plus one server op, and both frontends now route it through the durable queue `src/pending_ops.rs` (#0039).
`apply_move` / `apply_delete` / `apply_set_read` / `apply_set_flagged` commit the local write and the owed `ServerOp` (defined in `src/ops.rs`, the library home of the remote op) in one transaction, so a crash between the halves can never lose the op nor leave the store optimistically changed with nothing owed.
The TUI's `src/tui/mutations.rs` `queue_*` functions call those `apply_*` and return the rows they touched for the list update; the TUI keeps no server thread and no rollback of its own, because the queue owns both.
The background `drain` retires confirmed ops and rolls failed ones back under the engine lock, and it runs at the sync/fetch resume points beside `resume_outbox` (`pending_ops::resume_account`), draining nothing and building no backend when no row is owed.
Replay is exactly-once for the local half because the drain runs only the server op and never re-applies the local change, and it converges a crash-replayed not-found rather than failing it.
The CLI (`mp archive`, `mp delete`) enqueues through the same `apply_*` and then runs the op synchronously with `pending_ops::run_and_settle`, keeping its blocking UX: a success retires the row, a refusal rolls the local half back and returns the error verbatim, so a not-found stays byte-identical to the pre-queue message.
The synchronous settle deliberately does *not* converge a not-found, because a CLI invocation runs the op once in the process that enqueued it and so is never a crash replay.

### Send

`send::send_draft(&EmailDraft, &SendContext) -> SentDraft` is the one orchestration behind `mp send`, `mp send-approved` and both TUI send keys: it builds the bytes, commits the outbox row, submits over SMTP or Graph depending on which the context names, and retires the draft file.
Callers keep only what differs between them, the confirmation prompt, the wording of the result and the exit code.

A reply or forward draft names its source in `in_reply_to:` / `forwarded_from:`, and `send::mark_source_after_send` is the one reader: after a successful submission it flags every local copy of that source `\Answered` or `$Forwarded` and enqueues the server half on the durable queue as a single `ServerOp::SetAnswered` naming every server folder the source is filed in (#TKT-0051, #0076).
The send path opens no IMAP session for this: it costs one `COMMIT`, and the drain writes the flag in every named folder over one session (`imap_client::add_flag_in_mailboxes`) at the next resume point.
Best effort throughout, which here is a durability statement: the enqueue happens strictly after delivery, touches no `outbox` row, and every error is logged and swallowed, so bookkeeping can neither fail nor re-send a message that already went out.
The op's rollback is `Rollback::None` on purpose: the answered bit records something that happened, so a server refusal is not a reason to un-say it; the next sync restates whatever the server holds.
A Graph account writes the local bit and queues nothing (answered lives in extended MAPI properties, #0042/#0055).

`src/send.rs` builds the message, then `DurableSend::begin` commits the raw bytes as a blob and a `pending_send` outbox row *before* SMTP opens.
Submission is per recipient: each recipient gets an individual envelope while the visible To and Cc headers are preserved for all, which gives per-recipient success and failure tracking.
`src/outbox.rs` owns the four-state machine (`pending_send`, `sent_pending_append`, `done`, `failed`) and the exactly-once marker: `submission_started_at` is committed immediately before the SMTP session opens, so a `pending_send` row found on restart says whether the transport was ever entered.
Rows that provably never reached it are resubmitted; rows that died inside it are parked in `failed` for a human and never auto re-sent.
The APPEND to the server's Sent mailbox is retried until acknowledged, and a retry first searches Sent by Message-ID so it cannot duplicate.
Accounts whose server files its own Sent copy (Gmail, Graph, Proton) skip the APPEND entirely.
A fully sent draft with a durable record behind it is removed from `drafts/`; anything less keeps its file.

Because SMTP runs once per recipient, a submission has one verdict per recipient rather than one verdict (#0063).
The verdicts are committed to the row's `envelope` column: `delivered` is what a retry skips, `rejected` is what the user is told about, and what is in neither is what the next pass attempts.
A recipient that answered 250 is therefore never spoken to twice, whatever else the pass did, and a 5xx stops that recipient instead of being retried forever.
A row with a recipient that gave no verdict at all is parked in `failed` as before; a row that reached some recipients and was refused by others reaches `done` and keeps a note in `last_error`, which is what keeps it listed by `mp outbox list` and counted in the TUI's outbox badge until it is discarded.

One draft is one submission at a time.
Every build mints a fresh `Message-ID`, so a second send of the same draft would look like an unrelated message to both the outbox and the Sent dedup search; the envelope therefore carries the draft key, `outbox::enqueue` refuses a draft that already has an open row, and `send_draft` holds a process-wide slot per draft so the TUI's cursor send and approved batch cannot both submit it.

## Sync backends

Two transports, one ingest path and one `SyncResult` shape: IMAP/SMTP for password and OAuth2 XOAUTH2 accounts, Microsoft Graph REST for tenants that block IMAP/SMTP (see [auth.md](auth.md)).
TUI actions branch on `app.is_graph()`.

The shared half is `src/sync/` (#0059): the sync types (`SyncTarget`, `SyncResult`, `FreshObservation`, `MailboxFetch`), the `SyncBackend` trait, and `sync::engine::run_sync`, which is the orchestration itself: skip lists, ingest, arrival marks, the #0074 ingest-failure bound, flags, cursors and the deferred prune pass.
`SyncBackend` has one method, `fetch_targets`, and takes `&mut self`, which is where a backend keeps what outlives a mailbox (a persistent session and its `HIGHESTMODSEQ`, #0041; a `deltaLink`, #0042).
The seam's first payoff is that the engine is driven by a fake backend in `src/sync/engine.rs`'s tests, offline, over the properties that used to be verifiable only against a live server.
The parity half of #0059 is parked with the Graph backend: `graph.rs` still runs its own loop rather than the engine.

### IMAP

The backend is `ImapBackend` in `src/imap_client/store_sync.rs`, and `sync_mailboxes()` is now the wiring that hands it and the store to `sync::engine::run_sync`.
IMAP allows one SELECTed mailbox per connection, so the mailboxes are fetched in parallel, each on its own session, up to `imap.fetch_concurrency` at once (default 4, clamped to [1, 8]); #0005.
The store reads that seed each fetch happen serially first, the network fetches overlap, and ingest runs serially in target order afterwards, so `buffered` (which preserves input order) keeps the #0072 prune ordering and the single-writer SQLite discipline intact.
Per mailbox, `UID SEARCH ALL` gives the UID list, the last `limit` UIDs are the window, pass 1 fetches `(UID FLAGS)` over the whole window and pass 2 downloads `BODY.PEEK[]` only for UIDs the store does not hold.
The store answers "which UIDs do I hold" with one query, so there is no local scan and no dedup pass.
IMAP supports implicit TLS (port 993) and STARTTLS (any other port, for example 1143 for Proton Bridge); the `ImapStream` wrapper injects a fake greeting for STARTTLS because `async_imap` expects one.

### Graph

The client and its orchestrator are `src/graph.rs`.
The folder enumeration returns every message's `internetMessageId`, read flag and received date; the messages the store does not hold are downloaded by id, twenty per `/$batch` call, newest first so a capped pass still takes the arrivals a user is waiting for.
Graph never returns RFC822, so rows get `raw_blob` NULL and the HTML part is stored as an `html` blob instead.
Graph has no UID, so the row's `uid` is a 63-bit hash of the Message-ID (`ingest::graph_uid`), which keeps the `(account, mailbox, uid)` identity meaningful.
Since #0055 the orchestration mirrors the IMAP one line for line, prune pass included.
The enumeration is keyed on the trimmed `internetMessageId`, walks the folder newest-first, and reports whether it saw all of it; #0065 turned that report into the prune's precondition.

### Watchers

One IMAP IDLE thread per password or OAuth2 account, and one polling thread per Graph account that compares the *set* of inbox ids rather than its cardinality.
Both emit `WatchEvent::{Changed, Reconnected, Error}` on a shared channel tagged with `account_index`, and both widen their retry interval after consecutive failures instead of hammering a server that is down.
Changes on a non-active account set `has_unseen`, which is the badge in the status bar.

## Module map

| File | Responsibility |
|------|---------------|
| `src/types.rs` | Shared types: `EmailStatus` (the three draft states), `MessageFlags` (the received-mail status axis: seen, answered, forwarded), `MailboxRole` (the store's mailbox key), `EmailFrontmatter`, `EmailDraft`, `EventFrontmatter`, `collapse_hyphens` |
| `src/config.rs` | Config loading (`~/.config/mailypoppins/config.toml`), `config_dir` + the one-time #0022 legacy move, secrets-backend dispatch, data dir helpers (`mailypoppins_data_dir`, `account_dir`, `store_path`, `blobs_dir`, `drafts_dir`, `tokens_dir`, `logs_dir`, `contacts_cache_path`), legacy-config rejection, logging init |
| `src/secrets.rs` | Machine-bound encrypted secrets store (ChaCha20-Poly1305 + HKDF-SHA256). `SecretsBackend` trait with `EncryptedFileBackend` (default) and `KeyringBackend` (opt-in). See [secrets.md](secrets.md). |
| `src/oauth2.rs` | OAuth2 device-code flow, encrypted token cache at `tokens_dir()/<account>.enc`, refresh, XOAUTH2 SASL builder. Scope-parameterised (`IMAP_SMTP_SCOPES` vs `GRAPH_SCOPES`). |
| `src/ingest.rs` | The receive-path writer: fetched message to one `messages` row plus blobs, FTS maintenance, cursors, `prune_vanished`, `apply_seen_flags`, `graph_uid` |
| `src/selector.rs` | The `mp://account/mailbox/key` grammar: parse, resolve, format. Namespace fixed by the command, never sniffed. |
| `src/dump.rs` | `mp dump-mailbox`: path-free NDJSON envelope dump of the store, the parity harness for the data-layer rewrite |
| `src/read_cmd.rs` | `mp show` and `mp list-messages` (#0062): the human read surface over `store::read`, offline, rendering to a `String` so the layout is testable. Not the dump: that is an oracle with a pinned record shape. |
| `src/cutover.rs` | `mp cutover` (#0040): the end of the file-era transition. Mints an `id:` into any draft that has none (the one-time draft "import"; the drafts directory never moved) and reports the dead file-era mailbox directories. Deletes nothing, by design. |
| `src/reconcile.rs` | iMIP invite reconciliation, folded over the rows at display time and never persisted |
| `src/parse.rs` | RFC822 parsing, attachment extraction and sanitisation, `open_file_with_system()`, `materialisation_dir()`, `stable_attachments_dir()`, `ensure_utf8_charset()` |
| `src/draft.rs` | Draft parsing and validation, reply and forward creation (`create_draft_from_source`), `source_from_row`, status transitions, `settle_sent_draft` |
| `src/send.rs` | `markdown_to_html`, message building, `send_draft` + `SendContext`, per-recipient submission, `DurableSend`, `resume_outbox` |
| `src/outbox.rs` | The durable send state machine and its blob refcounting |
| `src/ops.rs` | `ServerOp` (the remote half of a mutation) and its IMAP/Graph execution seam `run_op`, at library layer so the durable queue and the CLI can drive it without depending on `tui/` |
| `src/pending_ops.rs` | The durable mutation queue (#0039): atomic local-write-plus-enqueue, the drain with backoff and per-kind rollback, crash-replay, `resume_account` (sync-tick drain) and `run_and_settle` (the CLI's synchronous single-op path) |
| `src/engine_lock.rs` | One engine per account across processes (#0061): a non-blocking `flock` on `<account_dir>/store.lock`, released on exit or crash |
| `src/graph.rs` | Microsoft Graph REST client: folders, fetch, sync, send, move, delete, read flags, search |
| `src/calendar.rs` + `src/invite.rs` | iCalendar receive-side parsing and send-side building |
| `src/contacts/` + `src/contacts_cmd.rs` | Contact index built from `messages` rows, frecency ranking, per-account cache at `account_dir(name)/contacts-cache.json`. CLI: `mp contacts {rebuild,stats,list}`. |
| `src/config_cmd/` | Config subcommands: init wizard, add-account, show, set-password, oauth2-login, reset-secrets, path |
| `src/calendar_cmd.rs` | `mp calendar rebuild`: reports what the invite fold resolves, writes nothing |
| `src/notify.rs` | Desktop notifications for new mail, shelling out to `osascript` / `notify-send` |
| `src/sync_health.rs` | `SyncHealth`, the per-account outcome of the last sync, plus the `mp sync` failure summary and exit code (#0071) |
| `src/timing.rs` | `TimingSpan`, which emits `[TIMING]` log lines with millisecond precision. Filter logs with `rg '\[TIMING\]'`. |
| **`src/sync/`** | |
| `mod.rs` | The transport-independent sync types and the `SyncBackend` trait (#0059) |
| `engine.rs` | `run_sync`: the orchestration every backend is driven through, plus `mark_below_unmet` (#0074) and the fake-backend engine tests |
| **`src/store/`** | |
| `mod.rs` | `Store`: the file, the pragmas, the drop-and-rebuild contract |
| `schema.rs` | Schema v6 SQL, version stamping, required-table validation, and the identity notes |
| `read.rs` | Listings, counts, Message-ID lookup, body and HTML loading, `materialise_attachments` |
| `write.rs` | The optimistic local half of a flag, move or delete |
| `drafts.rs` | The derived index over `<account_dir>/drafts/` |
| `blobs.rs` | The content-addressed blob store and its refcount discipline |
| **`src/imap_client/`** | |
| `mod.rs` | `ImapStream` wrapper, `open_imap_session()`, re-exports |
| `fetch.rs` | `fetch_new_raw_on_session` (the two-pass store fetch), `vanished_uids`, `fetch_emails*`, the arrival-coverage arithmetic |
| `store_sync.rs` | `ImapBackend` (the `SyncBackend` impl: the parallel per-mailbox fetch), `sync_mailboxes()`, `list_mailboxes()` |
| `search.rs` | `parse_search_query()`, `build_imap_search_query()`, `FetchCriteria` |
| `watch.rs` | `watch_mailbox()` (IMAP IDLE) |
| `ops.rs` | Single-message server ops: move, delete, read flags |
| `batch.rs` | `batch_move_on_server`, `batch_delete_on_server` |
| `sent.rs` | `ImapSentMailbox`: the APPEND seam the outbox drives, faked in tests |
| **`src/tui/`** | |
| `mod.rs` | Event loop (`run_loop`), watcher spawn, background result drain |
| `actions.rs` | `handle_action()`, the side-effect dispatch for all `Action` variants. Branches on `is_graph()`. |
| `mutations.rs` | The TUI's `queue_*` entry into the durable mutation queue (#0039): local write plus enqueue, testable without a terminal |
| `bg.rs` | `handle_bg_result()`, processing background task completions |
| `helpers.rs` | Terminal suspend and resume, editor, clipboard, the two watcher loops, `lib_do_sync`, `lib_do_sync_graph`, `resolve_send_account` |
| `event.rs` | Crossterm event polling |
| `theme.rs` | Named themes, semantic colour slots |
| **`src/tui/app/`** | |
| `mod.rs` | `App` struct, `new()`, `update()`, account sync, core state helpers |
| `types.rs` | `EmailEntry`, `AccountState`, `BgResult`, `Action`, `Focus`, `MailboxKind`, `open_store`, mailbox builders |
| `keys.rs` | `handle_key()` dispatch and all `handle_*_key()` methods |
| `keymap.rs` | The single `KEYMAP` table behind the help overlay, the hint bar and `mp dump-keys` |
| `calendar_view.rs` | Agenda rows built from the iMIP messages the store holds |
| **`src/tui/ui/`** | |
| `mod.rs` | `view()`, the top-level layout dispatch |
| `views.rs` | View switcher chrome |
| `sidebar.rs`, `list.rs`, `headers.rs`, `preview.rs`, `compose.rs`, `status.rs`, `activity.rs` | Mail view panes |
| `calendar.rs`, `contacts.rs` | The other two views |
| `overlays.rs`, `search.rs` | Confirm dialog, attachment picker, persistent error, help overlay, server search |
| `widgets.rs`, `util.rs` | Shared widgets, `pane_border_style`, `hint_span`, `truncate` |

## TUI layering

- The TUI follows The Elm Architecture.
`App::update()` is a state machine (`Message -> State`).
Side effects are dispatched as `Action` variants and executed in `tui/actions.rs::handle_action()`.
Background operations run on threads and report back over an `mpsc` channel as `BgResult` variants, each tagged with `account_index`.
- `ui/` renders from `App` state only.
It opens no store, runs no SQL and performs no I/O.
- `app/` is not pure in that sense.
It opens the account store synchronously to load listings, counts, drafts and the preview body, through `open_store` in `app/types.rs` and nine other call sites across `app/mod.rs` and `app/types.rs`.
Those reads are local, indexed and memoised, so they cost little today, but a new one is a synchronous disk hit inside the update pass and belongs behind an `Action` if it can be slow.
What stays absolute is the protocol boundary: no SMTP, IMAP, MIME or Graph code in `app/` or `ui/`.
- Account state proxy pattern.
`App` holds a `Vec<AccountState>` plus top-level proxy fields (mailboxes, list index) that mirror the active account, with `save_to_account()` and `load_from_account()` syncing on switch.
This avoids routing every key handler through indirect access.
- Mutations are optimistic: local state and store update immediately, the server op is retired by the durable-queue drain at the next sync/fetch resume point, and a refusal rolls the row back there (#0039).

## Multi-account

Config uses an `[[accounts]]` array.
Each account has independent IMAP/SMTP settings, mailbox mappings and signatures, and its own store, blob directory and secrets keys (`smtp-password-{name}`, `imap-password-{name}`).
The TUI shows one account at a time, switching via backtick or Ctrl+1-9, and watches all of them for new mail simultaneously.
CLI commands target an account via `--account` and default to the first.

## Selector contract

No CLI input position takes a filesystem path (#0050).
A message is named by `[mp://<account>/][<mailbox>/]<key>`, and the canonical form every command prints is the fully qualified `mp://<account>/<mailbox>/<key>`.
Elision is positional: without the scheme, the account comes from `-A/--account` or the default account and the mailbox from `--mailbox` or the command's declared default scope.
The namespace (received mail or drafts) is fixed by the command, never sniffed from the string, so a Message-ID that happens to look like a draft id cannot be reinterpreted.
Resolution is one indexed lookup; an ambiguous key lists every candidate and asks for `--mailbox` rather than picking one.

## Performance-critical invariants

These exist for measured reasons; do not regress them without re-measuring.

- **Pass 1 covers the full window.**
The flags it collects are the only server-to-local channel for the whole status axis (`\Seen`, `\Answered`, `$Forwarded`), so any "probe fewer UIDs first and bail early" optimisation silently breaks read/unread sync and the answered/forwarded state with it.
This happened once already (#0004).
IMAP states the whole flag set and is truth for all three bits (`ingest::apply_flags`); Graph knows only `isRead` and merges that one bit in (`ingest::apply_seen_flags`), so a Graph pass cannot erase an `\Answered` no Graph call can restate.
- **The prune is clamped to the window's UID range.**
`UID SEARCH ALL` returns the whole mailbox but the window is only its newest `limit` UIDs, so only a known UID *between* the window's lowest and highest is provably gone from the server.
Negative UIDs (the local-move sentinel) and hash-sized UIDs (an APPEND with no `APPENDUID`) fall outside by construction.
- **The Graph prune runs only on a pass that saw everything, and never on a fresh row.**
Graph enumerates the whole folder, so there is no UID range to clamp to; what stands in for the clamp is that every target must have enumerated in full and downloaded its whole backlog before any prune applies (a capped quick sync defers them), and that a row dated within `ingest::PRUNE_MIN_AGE_SECS` of now is skipped.
The age window is what keeps the prune from deleting the copy of a just-sent message, which the store files under our own Message-ID and the server lists under one of its own (#0065).
- **Prunes run after every target is ingested.**
Targets sync in order, so pruning inside the loop would delete the inbox row of a message archived elsewhere before the archive pass ingests it, leaving a window with no row anywhere and blobs dropping to refcount zero.
Both backends hold their prunes back for this reason.
- **The integrity check is amortised.**
It is a full walk of the file, so it runs once per file per process, not once per open.
- **Read-flag updates land in one transaction per mailbox**, not one commit per message.
- **Queued mutations.**
A mutation enqueues into the durable `pending_ops` queue and applies locally at once, spawning no background job.
Nothing defers a fetch or sync behind it any more: #0039 retired the mutation-count gate and the "Quick sync queued (N ops pending)" stacking it needed, and #0076 removed the vestigial always-zero field it left behind.
The owed server op is drained at the next sync/fetch resume point.

## Data and config layout

User-owned config:

- The config file is `~/.config/mailypoppins/config.toml`, a multi-account `[[accounts]]` array.
  It is user-edited and references signature paths and account-level settings.
- The secrets file is `~/.config/mailypoppins/secrets.enc`, machine-bound encrypted (see [secrets.md](secrets.md)).

Both live under `config_dir()`, overridable with the `MAILYPOPPINS_CONFIG_DIR` env var, which mirrors `MAILYPOPPINS_DATA_DIR` and is what the CLI integration tests point at a tempdir.

The directory was `~/.config/email` before #0022, and `config::migrate_legacy_config_dir()` moves it once, at startup in `main()`, before anything reads config or secrets.
This does not contradict the no-migrations invariant: that invariant is scoped to data formats, secret storage and wire protocols, and a directory rename reads not one byte inside the directory.
A hard cut would instead have cost every stored SMTP/IMAP password.
The move is one `fs::rename` and nothing else, which is what makes it idempotent and safe under two concurrent `mp` invocations: the loser of the race gets `ENOENT` and treats old-absent plus new-present as success.
There is no copy fallback and, more importantly, no read fallback: a rename that fails names both paths and the exact `mv` to run and exits 1, because a client that quietly kept reading `~/.config/email` would never finish the move.
Setting `MAILYPOPPINS_CONFIG_DIR` skips the move entirely, since an explicit override must not carry a migration side effect.

App-managed data, all under `mailypoppins_data_dir()`:

| Platform          | Default `mailypoppins_data_dir()`                          |
|-------------------|------------------------------------------------------------|
| macOS             | `~/Library/Application Support/mailypoppins`               |
| Linux (incl. WSL) | `$XDG_DATA_HOME/mailypoppins` (def. `~/.local/share/mailypoppins`) |

Layout under the data dir:

```
<data_dir>/
  accounts/<name>/store.sqlite3          # the per-account store (plus -wal, -shm)
  accounts/<name>/blobs/ab/cd/<sha256>   # bodies, raw messages, attachments
  accounts/<name>/drafts/*.md            # the only local truth
  accounts/<name>/attachments/<message-id>/   # materialised for forward drafts (#0006)
  accounts/<name>/contacts-cache.json
  tokens/<name>.enc                      # OAuth2 / Graph encrypted refresh tokens
  logs/mailypoppins-YYYY-MM-DD.log
```

Nothing under `accounts/<name>/` is created eagerly: `mp config init` makes the account directory, the first sync makes the store and its blob directory, and the first draft makes `drafts/`.
Override the root via the `MAILYPOPPINS_DATA_DIR` env var, which tests use and which doubles as the escape hatch for a portable location.

`retention` is parsed and validated in config but not enforced yet: the blob store grows without bound until #0060 lands.

The OS keyring service name (when the keyring backend is opted into) is `mailypoppins` (constant `KEYRING_SERVICE` in `src/secrets.rs`).
It was `email-cli` before #0022, and `get` falls back to that name so a user who opted in before the rename is not locked out of stored credentials; `set` and `delete` touch the new service only, so the next `mp config set-password` migrates the credential and leaves a harmless stale entry behind.

## Testing

- **1011 tests**, run by `cargo test`.
All of them run offline in under a second.
- Unit tests are inline `#[cfg(test)] mod tests` in each module; integration tests live in `tests/` and use `tempfile::tempdir()` plus `MAILYPOPPINS_CONFIG_DIR` and `MAILYPOPPINS_DATA_DIR` for isolation.
- `insta` snapshots cover `markdown_to_html`, the whole `mp --help` surface (`tests/cli_help_snapshot.rs`) and the TUI golden frames (`src/tui/ui/golden_frames.rs`).
`cargo insta review` approves changes; a diff there is a decision, not an approval reflex.
- The store side is fixture-driven: `tests/store_ingest_integration.rs` ingests real RFC822 bytes and asserts rows, blobs, refcounts and FTS state; `tests/outbox_integration.rs` drives the state machine against a fake Sent mailbox.
- The sync engine (`src/sync/engine.rs`) is tested offline against a fake `SyncBackend` (#0059): ingest and cursor advance, the #0074 arrival mark and its give-up bound, the UIDVALIDITY reset, the deferred prune pass and its account-wide coverage gate, `dry_run`, and the flag application.
  `ops.rs` and `batch.rs` still have none: their seam is `ops::run_op`, not this one.
  The Graph orchestrator still has none either, because it does not run on the engine yet.
- There is no IMAP/SMTP mock server.
