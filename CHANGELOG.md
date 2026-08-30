# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Performance
- **The preview body is wrapped once, not every frame (#0093).** Parsing the
  inline markdown and word-wrapping the whole message into styled lines used to
  run on every render, so every scroll keystroke and every idle tick redid work
  proportional to the body length. The styled lines are now memoised, keyed by
  the body content, the pane width and the inline-image set, and only the
  scrolled window is rendered, so a scroll costs the visible height rather than
  the whole body. The cache rebuilds on a selection move, an async body
  arrival, a re-ingest under the cursor, and a terminal resize.
- **The TUI redraws only when something changed (#0093).** The event loop
  called `terminal.draw` on every iteration, roughly four times a second at
  idle, rebuilding all widget content each tick. It now tracks a dirty flag and
  skips the draw when nothing moved; input, resize, watcher events, background
  results and drafts changes all mark the frame dirty, and the busy spinner
  keeps its own slow tick, so idle CPU drops toward zero without stalling any
  background update.
- **The mailbox listing is served by an index, not a sort (#0094).** Loading a
  mailbox filtered by `(account, mailbox)` and sorted by `date_sort DESC, id
  DESC` with no index behind the sort, so SQLite built a temp B-tree on every
  load, and each row paid a correlated `EXISTS` subquery for its invite badge.
  A new `messages_list` index on `(account, mailbox, date_sort DESC, id DESC)`
  lets the listing walk straight off the index, and the invite flag now comes
  from a deduplicated `LEFT JOIN` rather than a per-row subquery, so a mailbox
  switch or a reload is one index scan with no temp sort. The listing content
  is unchanged; the schema version bumps to 7, which rebuilds existing stores
  from their cache on the next open.
- **Network actions share one tokio runtime instead of building a fresh one
  each (#0095).** Every background sync, send and search, plus the IMAP and
  Graph watcher threads, used to call `tokio::runtime::Runtime::new()` on their
  own thread and tear the whole runtime down when the op finished, spinning up
  a worker thread per core and a blocking pool every time. They now `block_on`
  a single lazily-built multi-thread runtime that lives for the process, so an
  action pays the work and not the thread-pool churn. Blocking semantics are
  unchanged: each call still blocks its own OS thread, and no path nests one
  runtime inside another.

### Added
- **Folder entries in `attachments:`.**
  A draft's `attachments:` frontmatter now accepts a directory path, not only individual files.
  A folder entry attaches every regular file directly inside it (sorted by name, subfolders and dotfiles skipped), so a batch of files can be named by one folder rather than listed one by one.
  File entries behave as before, and the two can be mixed in the same list.
  Draft validation warns when a named folder holds no files.
  The expansion is shared by the SMTP and Graph send paths.
- **One search grammar across every backend (#0086a).** `mp search` now speaks a
  single grammar that a single parser lowers to one AST and four renderers, so
  the server path and `mp search --local` finally read the same input (the
  #0043 two-grammar debt is closed). New in the grammar: `has:attachment`,
  `(a OR b)` groups and bare `a OR b`, `filename:`, quoted phrases everywhere,
  and custom `after:` / `before:` dates (`since:` aliases `after:`); the old
  `in:` and `message-id:` directives still work. Every field also has a flag
  (`--from --to --cc --subject --body --filename --has-attachment --after
  --before`) that builds the identical query, so
  `mp search --from boss@corp.com --has-attachment 'invoice OR receipt'` equals
  `mp search 'from:boss@corp.com (invoice OR receipt) has:attachment'`. On Gmail
  (`X-GM-RAW`) and Microsoft Exchange (`$search`/`$filter`) every term including
  the attachment test runs server-side; plain IMAP (RFC 3501) has no attachment
  search key, so that residue is answered from the local store's synced mail and
  the run prints a warning that un-synced mail is not covered. A malformed query
  is now an error with a caret pointing at the problem, never a silent search
  for fewer conditions. Two behaviour changes fall out of parentheses and `OR`
  becoming grammar: a bare multi-word query is now AND-ed per word rather than
  matched as one contiguous IMAP `TEXT` phrase (quote it to keep the phrase),
  and `--local` refuses `to:`/`cc:`/`filename:` (not indexed) with a clear
  message instead of searching them as literal text. The TUI's server-search
  overlay inherits the richer grammar for free.
- **Outlook-shape TUI search form (#0086b).** The server-search overlay (`f`) is
  now a field form instead of one line: a `Search In` scope toggle (Current
  Mailbox / Current Account, per-account only, no All-Accounts), `From`, `To`,
  `Subject`, `Keywords` (free text incl. `a OR b` groups and quoted phrases),
  custom `After` / `Before` date fields, an `Attachment` toggle, and an
  `Advanced` raw-grammar line that accepts the full #0086a grammar and surfaces
  its parse error verbatim. Tab / Shift+Tab cycle the fields, Space flips the
  toggles, Enter searches, Esc closes, reusing the compose wizard's
  conventions. A non-blank `Advanced` line takes over and greys the structured
  fields; otherwise the structured fields build a `search::Query` AST directly
  (no string concatenation) through the same lowering path the CLI uses. An
  empty form is a no-op. No new key binding: `f` still opens the overlay.
- **Retention enforcement (#0060).** The `[retention]` disk cap is now acted on:
  a sweep runs after every `mp sync` and on demand via `mp store gc`. It is the
  first code path in mailypoppins that deletes user data, so every deletion
  decision is pinned by a test. The store is a cache, so eviction removes only
  cached blob *files* (and their refcount rows), never a `messages` row: the
  message list stays complete and a re-ingest re-materialises an evicted body.
  A two-strike marker makes the first over-cap sweep *warn only* (`store at X /
  cap Y, will prune on next run`) and persist a store-level marker; the next
  over-cap sweep evicts, and dropping back under the cap clears the marker.
  Eviction order is age horizon first (attachments then bodies past their
  horizon), then attachment blobs oldest-first, then body blobs oldest-first,
  stopping the moment the store is back under the cap; a blob a message still
  references inside its horizon survives. `mp store gc --dry-run` prints what
  would go; a sweep that would reclaim more than half the store's blob bytes is
  refused without `--force` (a fat-finger guard while on-demand re-fetch of an
  evicted body, #0085, does not yet exist). The default cap is 10 GB
  (per-account `max_disk_bytes` overrides still win), and `mp config show` now
  reports retention as enforced. Raw RFC822 blobs are not evicted (the order
  names only attachments and bodies).
- **Pane zoom (#TKT-0044).** `z` gives the focused pane the whole content
  area, herdr-style, and `z` again restores the split -- a zoomed email list
  gains its contact column back, a zoomed preview reads at full width. The
  zoom follows the focus, so `Tab` under a zoom moves it to the next pane
  instead of stranding one, and the hint bar's badge says `BODY ZOOM` so the
  hidden panes are never mistaken for empty ones. The hint and status rows are
  never hidden by it. Mail view only: Contacts and Calendar are a list and a
  detail card that already resize themselves, and `z` is swallowed there.
- **Inline images in the preview pane (#0010).** A message whose HTML body
  points at its own image parts with `cid:` URLs now shows them as pixels, not
  as nothing: the terminal is asked once at startup what it can draw
  (`ratatui-image`'s capability query -- kitty, iTerm2 or sixel), and each
  referenced image is decoded once per cursor move and painted into rows the
  text flow reserves for it. Only inline-referenced images are drawn; an
  attached photo the body never mentions stays an attachment. Every terminal
  that cannot draw pixels keeps exactly the pane it had, plus a
  `[image: filename]` line per image -- halfblocks were rejected on purpose,
  and no escape byte is emitted anywhere that has not said it understands one.
  A Graph row (no RFC822 to walk), an undecodable part, and anything over 8 MB
  or 40 megapixels degrade to the same placeholder, per image.

### Added
- **iMIP cancellations and updates, receive side (#0031).** A `METHOD:CANCEL`
  now does something: the event it names is marked cancelled everywhere it is
  shown -- the red "Cancelled by the organizer." banner leads the shared event
  card in the mail preview and the Calendar detail, and the agenda row keeps
  its `cancelled` badge. It is a tombstone, never a deletion: the invite, its
  `invite.ics` and every field on the card stay readable, because an event the
  organizer called off is still something the user may want to look at. A
  re-issued invite with a bumped `SEQUENCE` supersedes the copy already
  stored, and the older copy says so on its card instead of pretending to be
  current. Identity is `(UID, RECURRENCE-ID)` and the version chain is
  `(SEQUENCE, DTSTAMP)`, so a CANCEL naming one occurrence of a series kills
  that occurrence only (the series row lists it and lives on), and a replayed
  or re-delivered copy at an equal or lower version can never clobber newer
  local state. `V` refuses to RSVP a cancelled or superseded version rather
  than mailing an answer the organizer has already moved past. All of it is
  derived on every pass from the stored `invite.ics` blobs and never
  persisted, so arrival order is irrelevant: a CANCEL that reaches the mailbox
  before its invitation applies exactly the same. A malformed or UID-less
  CANCEL costs itself and nothing else. Send-side updates and cancellations
  (re-sending with a bumped `SEQUENCE`) remain open as #0084.

### Added
- **Jump to a date in the mailbox list (#0017).** `g t` arms an inline
  `date:` prompt over the list and Enter moves the cursor to the newest
  message on or before what was typed: `2024-03-07`, `2024-03`, `2024`,
  `today`, `yesterday`, `last week`, `2 months ago`. Nothing is filtered --
  the rows above and below stay exactly where they were, which is the
  difference between this and `/` -- and the jump is a binary search over the
  already date-sorted list, so a mailbox with thousands of rows costs the same
  as one with ten. A date older than the whole mailbox parks on the oldest
  message and says so; a date the grammar cannot read leaves the prompt up
  with the accepted forms on the status line. The grammar is closed on
  purpose: no natural-language date library, because a wrong guess would
  silently land the cursor in the wrong year.

### Added
- **`o` opens a draft's attachments (#0016).** The key only ever worked on
  received mail; on a draft it declined with a status line pointing at the
  `attachments:` list it would not read. It reads it now: the paths in the
  draft's own frontmatter, `~` expanded the way the send path expands it, so
  what `o` opens is the file that will be sent rather than a temp copy of it.
  Zero, one and many attachments behave as they do for received mail (nothing
  to show says so, one file skips the picker, several open it), in List,
  Headers and Preview focus, and `O` saves them for the same reason. A listed
  path that is no longer on disk is named on the status line instead of being
  skipped silently -- that stale path is the `mp send` failure the key is being
  pressed to find out about. No binding changed, so the help overlay and the
  website key table are untouched.

### Added
- **`mp search --local`, ranked full-text search over the store (#0043).**
  Search is a `SELECT` now, not a stream over files: `mp search --local
  <query>` answers from the store's FTS5 index, offline, over every synced
  mailbox of the account at once, best match first. `--mailbox` narrows it to
  one mailbox, `-n` caps the hits and `--full` prints the bodies. The
  server-side `mp search` is untouched and stays the default; the TUI's `\`
  body filter is also untouched, and stays the substring filter it has been
  since #0038 (the reasoning is in `src/tui/app/types.rs`).

  The query is translated rather than passed through: every term becomes a
  quoted FTS5 literal, so `c++`, `(draft)` and a stray quote are searched as
  the text they are instead of failing as syntax. `"a phrase"` matches
  adjacency, a trailing `*` is a prefix, and `subject:`, `from:` and `body:`
  restrict a term to one column. Ranking is bm25 with the subject weighted
  above the sender and the sender above the body, so a word in a subject line
  outranks the same word buried in a quoted reply chain.

  No schema change and no new maintenance: the index has been written inside
  the same transaction as the `messages` row since #0038 and removed by every
  delete path (re-ingest, `mp delete`, the TUI's `d`, the sync prune), which
  the new tests now assert from the query side, through
  `store::search::index_drift`. A whole-account search over a 712-message
  store answers in ~20 ms.

- **Microsoft Graph incremental sync via `/messages/delta` (#0042).** A Graph
  account used to enumerate every message in every folder on every pass, just
  to work out what was new. A quick sync now walks `/messages/delta` from a
  `deltaLink` persisted per folder in `sync_cursors`, downloads only the
  messages that changed, and applies read-status from the same change set.

  The token asserts one thing, "at the moment it was minted, the store held
  every message the folder listed", and every rule around it exists to keep
  that true: it is minted with `$deltatoken=latest` *before* the enumeration it
  is stored alongside, only by a pass that saw the whole folder and wrote every
  message in it, and only together with the folder id it is bound to (Graph's
  UIDVALIDITY equivalent, so a folder deleted and recreated under the same name
  drops its token). A full `mp sync` always relists, which is the periodic
  whole-folder observation the prune leans on; a 410, a 404, an unparseable
  page, a page cap and a chain that ends without a resume point all throw the
  token away and enumerate in the same pass. Deletions are deliberately not
  taken from the delta: a `@removed` entry names the message by Graph id and
  the store keys Graph rows on `internetMessageId`, so a pass whose delta
  reports a removal escalates to the full enumeration and the prune keeps its
  existing source of truth with the #0065/#0072/#0074 coverage gates on
  unchanged inputs. The #0074 ingest bound applies to the token as well as to
  the prune, so a message the store cannot write cannot wedge the delta chain.

  Not yet verified against a live tenant: no Graph account is configured on the
  machine this shipped from, and the acceptance run is split out as #0082. Every
  fallback above is the pre-#0042 pass, so an assumption that turns out wrong
  costs a full enumeration rather than a missed message.

- **`mp cutover`, the end of the file-era `.md` layout (#0040).** Received mail
  used to be one Markdown file per message under the account directory; it has
  lived in the SQLite store since the data-layer rewrite, and the old
  `inbox/`, `archive/`, `sent/` and slugified-mailbox directories have just
  been sitting there since. `mp cutover` closes the transition: it mints an
  `id:` frontmatter field into any draft that has none -- the one-time draft
  import, and the only thing in the old tree the server cannot give back --
  so that draft resolves through a selector and shows up in `mp list`, and it
  then names every dead directory with its `.md` count and size and prints the
  `rm` line that removes it. It deletes nothing itself and runs only when
  invoked: there is no migration on startup. Re-running it writes nothing (the
  ids are already there), and `--dry-run` writes nothing at all.

  This also closes TKT-0047 by construction: the reconcile walk that could be
  fed a forged `method: REPLY` from a sender-controlled `.md` attachment is
  gone, invite statuses are folded from `invite.ics` blobs of message rows, and
  a regression test now pins that a forged attachment cannot move a `PARTSTAT`.

- **`mp show` and `mp list-messages`, a read surface over the store (#0062).**
  After a sync, received mail was reachable only through the TUI or through
  `mp dump-mailbox`, which is a parity oracle emitting NDJSON envelopes rather
  than something to read. `mp list-messages` prints one line per stored message
  with its full selector -- one mailbox with `--mailbox`, otherwise every
  mailbox of the account grouped in sidebar order with `-n/--limit` applied per
  mailbox -- in the order the TUI list shows. `mp show <selector>` prints the
  headers, the attachment list and the body of one message, taking the same
  selector grammar as every other message command (a bare Message-ID is enough
  when it is unambiguous, and a selector naming another account reads that
  account's store). Both are offline: they open no connection and answer from
  the store alone. A body the store no longer holds prints as a note rather
  than an error, and `mp show --json` emits one object whose body is a JSON
  string, so a body that opens with `---` cannot be misread as frontmatter.

- **A flagged-only view in the mail list (#0079).** `F` narrows the list to
  starred messages and widens it back, finishing the half of #0007 that shipped
  the `\Flagged` round-trip and its marker but not the filter. It is a local
  read-side view over rows already loaded, so it costs no server call: the flag
  bit is in the store. It composes with the `/` search rather than replacing it
  (what you see is the intersection), the list title names whichever narrowings
  are on, and the status line keeps showing how many of the mailbox's messages
  are visible.

- **Durable mutation queue and single-engine lock, the #0039 core (internal).**
  Archive, delete, move and mark-read now have a durable home (`src/pending_ops.rs`)
  that commits the local store change and the server op it owes in one
  transaction, so a crash between the two can no longer lose a flag change or
  strand an optimistic move. A background drain retires confirmed ops, backs off
  transient failures, and past a retry budget rolls the local state back to the
  server's and surfaces the failure. A non-blocking `flock` on
  `<account_dir>/store.lock` (`src/engine_lock.rs`) makes at most one process the
  engine for an account, so `mp sync` and an open TUI cannot double-drain. This
  ships the durability plumbing only; the TUI and CLI still mutate the way they
  did, and wiring them onto the queue is the follow-up half of #0039. No
  user-visible behaviour changed.

### Removed
- **The file-era invite rewriters (#0069, internal).** `draft::set_event_rsvp`
  and `draft::set_event_attendee_status` edited an invite's `.md` frontmatter in
  place, which is the shape of the receive path the store cutover deleted; RSVP
  state has come from store rows since #0038, and neither function had a caller
  outside its own tests. They are gone with `AttendeeUpdate`, the two YAML
  helpers only they used, and `types::InboxFrontmatter`, whose one surviving
  reader (the Markdown-rendition format-parity test) now owns it as a fixture.
  The TUI RSVP flow is unchanged.
- **Two dead seams left by the mutation-queue wiring (internal).** `App`'s
  always-zero `bg_mutations` field and its per-account save/restore are gone:
  mutations stopped being background jobs when #0039 wired the queue, so the
  counter had nothing left to count. `ops::run_ops` and its `homogeneous`
  batch-detection helper are gone too: the durable queue drains one row at a
  time through `run_op` and no consumer ever called the batch form. The
  `imap_client` batch primitives it wrapped (`batch_move_on_server`,
  `batch_delete_on_server`) stay, so a future multi-op drain has something to
  build on.

### Changed
- **The test harness no longer mutates the process environment (#0077).**
  Fixtures pointed `MAILYPOPPINS_DATA_DIR`, `TMPDIR`, `HOME` and
  `MAILYPOPPINS_CONFIG_DIR` at a tempdir with `std::env::set_var` and restored
  them on drop, guarded by a crate-wide mutex. The mutex only serialised the
  *writers*: every other test thread was concurrently reading the same
  environment through `getenv` (`tempfile::tempdir()` reads `$TMPDIR`), which
  is a data race on `environ` in a multi-threaded process rather than merely an
  unsynchronised read, and it forced every data-dir test to run one at a time.
  The overrides are thread-local now (`config::test_env`), so a fixture's paths
  are invisible to every other test, no lock is needed and the tests stay
  parallel. Materialised message files resolve through `parse::test_temp_root`
  instead of an overridden `$TMPDIR`. No shipped code path changed: the seams
  are `#[cfg(test)]`.

- **IMAP sessions are persistent and shared, not one per operation (#0041).**
  Every IMAP operation used to open its own connection: archiving three
  messages meant three TCP handshakes, three TLS handshakes and three LOGINs,
  and a sync of three mailboxes meant three more. `src/imap_client/pool.rs`
  now keeps authenticated sessions for the life of the process, keyed by
  server and user; syncs, queued mutations, batches and searches borrow one
  and give it back. This rewrites the one-session-per-operation invariant in
  `docs/architecture.md`, with the owner's approval. Reuse is guarded rather
  than assumed: every borrower re-`SELECT`s, a session idle over 20s is
  `NOOP`-probed before it is trusted and one idle over 10 minutes is dropped,
  connecting retries with backoff, and an operation that failed poisons its
  session instead of returning it, so a half-read response can never be
  misread as the next borrower's answer. The IDLE watcher keeps its own
  dedicated connection, because IDLE blocks the one it runs on.

- **CONDSTORE flag deltas where the server advertises them (#0041).** On a
  server with CONDSTORE (Gmail, Dovecot), a sync now `SELECT`s with
  `(CONDSTORE)`, remembers the mailbox's `HIGHESTMODSEQ`, and asks pass 1 for
  `(UID FLAGS) (CHANGEDSINCE n)` instead of restating every flag in the
  window. The gate is unanimous and strict, because a missed flag change is
  invisible: no CAPABILITY, no `HIGHESTMODSEQ` in the SELECT response, no
  stored resume point, a UIDVALIDITY change, or a server whose modseq went
  backwards all fall back to the full-window flag fetch, which is what keeps
  #0004 fixed. A resume point is only recorded by a pass that saw the whole
  mailbox, so a capped quick sync can never narrow what a later full sync
  asks about. Proton Bridge advertises neither CONDSTORE nor QRESYNC and takes
  exactly the path it took before, unchanged. QRESYNC and UIDPLUS are split
  out to #0081.

  Review follow-up: "saw the whole mailbox" had one hole. The early return for
  an empty window asserted whole-mailbox coverage unconditionally, so
  `mp sync -n 0`, a prune-only pass that fetches no flags at all, recorded
  the server's `HIGHESTMODSEQ` anyway, and the next `CHANGEDSINCE` started past
  every flag change made in between. Both return paths now ask one function,
  `window_is_whole_mailbox(window_len, listed_len)`: an empty window covers
  only an empty mailbox. Confirmed live against Gmail, where a `-n 0` pass with
  a stale stored resume point left it stale (the pre-fix binary advanced it).
  Separately, a batch move or delete whose trailing `EXPUNGE` fails now poisons
  the pooled session: the caller's ops still succeeded, but a half-read EXPUNGE
  leaves bytes in the stream that the next borrower inside the 20 s probe
  window would read as its own answer.

- **The sync orchestration moved behind a `SyncBackend` trait (#0059).** The
  loop that ingests a pass, keeps the #0074 arrival mark, applies flags,
  records cursors and defers the prune used to live between the IMAP calls in
  `imap_client::store_sync`, which is why none of it had a test: driving it
  meant standing up an IMAP server. It now lives in `sync::engine::run_sync`,
  the transport is one trait method (`fetch_targets`), and
  `imap_client::ImapBackend` is its first implementation, with
  `sync_mailboxes` as the wiring around it. The sync types (`SyncTarget`,
  `SyncResult`, `FreshObservation`, and the fetch result now called
  `MailboxFetch`) moved out of `imap_client` into `crate::sync`, so `graph.rs`
  no longer imports them from the other transport's module.

  Nothing about a sync behaves differently: same phases, same order, same
  parallel per-mailbox fetch. What is new is that the engine is now driven by a
  fake backend in tests, offline, and the properties that were previously
  asserted by re-walking the loop's calls by hand (the arrival mark under an
  unwritten UID, its three-pass give-up bound, the failure counts a UIDVALIDITY
  reset clears) are pinned through the real loop instead, alongside new
  coverage of the deferred prune pass, its account-wide gate, `dry_run` and the
  flag application.

  The Graph backend is unchanged and still runs its own copy of the loop: the
  IMAP/Graph parity half of #0059 stays parked with the Graph backend itself.

### Fixed
- **A hand-written numeric `id:` in a draft is rejected loudly instead of
  silently re-identifying the draft (#0083).** `id: 123e456` or
  `id: 1234567890123456` typed into frontmatter by `$EDITOR` or an agent is a
  YAML *number*, not a string, so the field read back as absent and the next
  drafts-index refresh minted a replacement into the file: the draft's identity
  changed under every selector and index row, with no error anywhere (this was
  #0077's root cause). Such a file is now skipped with the reason named --
  `frontmatter 'id:' is a number, not a string: quote it (id: "...")` -- through
  the existing #0080 skipped-draft path, so `mp list` prints the path and the
  TUI Drafts list shows the broken file rather than a re-identified one. The
  file is not rewritten. A quoted `id: "1234567890123456"` is a string and is
  honoured verbatim; nothing is coerced. `set_draft_id` now also writes the id
  double-quoted, so the round trip is shape-stable whatever the id contains.

### Fixed
- **The hint bar no longer truncates mid-word, and `d Delete` is back on the
  default-width frame (#0078).** The bar lays every hinted binding of the
  context out end to end on one line and ratatui clipped whatever did not fit,
  so at 120 columns the mail view ended `... Reply / Reply-al` and both
  `a Archive` and `d Delete` were gone: one long description silently cost the
  bindings to its right. `KeyBinding` now carries an optional `short` label
  beside `desc`, used by the hint bar alone -- `Enter / e Open (read-only)`
  rather than `Open in editor (mail read-only)` -- and the bar drops whole
  `keys` + label pairs that do not fit, marking the cut with an ellipsis
  instead of a half word. The help overlay (`?`) and `mp dump-keys` still show
  the full descriptions, and `website/src/data/tui-keys.json` regenerates
  byte-identical. The Contacts and Calendar rows now fit at 120 columns
  outright.

- **A draft id is no longer occasionally read back as a number, silently
  changing the draft's identity (#0077).** `drafts::new_id` minted 16 random
  hex characters and the drafts index writes them into YAML frontmatter
  unquoted, but a plain hex string is not always a YAML string:
  `8808e70039225152` is a float in scientific notation and a 16-digit id is an
  integer. About one id in a thousand had one of those shapes. A float-shaped
  id deserialised the `id:` field to `None`, so the next refresh minted a
  *different* id, the old selector stopped resolving and the draft's preview
  and index row went with it; an integer-shaped one failed deserialisation
  outright and the draft was dropped from the index as unparseable. Minted ids
  now start with a letter, which no YAML number can, and the round trip is
  pinned by a test over 2000 ids. This was the mechanism behind all three
  intermittent test failures in #0077, none of which was a temp-dir or env-var
  race after all.

- **A full sync no longer wipes the delta resume point a delta sync recorded
  (#0041).** `record_mailbox_cursor` wrote `highest_modseq` and `deltalink`
  unconditionally from the caller's struct, and every path that is not a delta
  fetch passes `None` for them. So the first ordinary pass after a CONDSTORE
  or Graph-delta one erased the token, the next pass found nothing to resume
  from, silently fell back to the full window with no error anywhere, and the
  delta would have flapped in and out of use forever. Both columns are now
  carried forward with `COALESCE`, `None` means "this pass has nothing to say"
  rather than "clear it", and the one path that must clear a modseq, a
  UIDVALIDITY reset, does so explicitly. Found in the #0054 review and fixed
  before the first real modseq was ever written; it protects #0042's
  `deltaLink` too.

- **A failed ingest now holds the prune gate shut (#0074).** A message the sync
  downloaded and then failed to write counted as covered, so the pass reported
  itself complete, persisted no arrival mark, and the next pass stood on a floor
  above a message the server still lists: the one remaining way the prune gate
  could open over a hole in the store. The mark is now derived from what the
  pass actually wrote, so an unwritten UID pulls it under itself and the gate
  stays shut until some pass writes the message. The retry is bounded by a new
  `ingest_failures` table (schema v6): after three failed passes the UID is
  given up on with a loud log line and stops holding the prune back, which keeps
  a message the store rejects every time from suspending the prune for the whole
  account for good. One unwritable message never stops the rest of the batch;
  the ingest loop continues past it as it always did. Schema v6 means existing
  stores are dropped and refilled on the next open, as every version bump does.

- **The give-up bound now covers Graph accounts too, and survives a UIDVALIDITY
  reset honestly (#0074 review).** The Graph sync path folded an ingest failure
  into its coverage tuple with no bound at all, so one message the store rejects
  every time kept an Outlook account's prune suspended for good: exactly the
  deadlock the bound closes on the IMAP side. It now runs the same
  `ingest_failures` counter, giving up loudly after three failed passes and
  clearing the count on every success. Separately, the counters are keyed by UID
  and a UIDVALIDITY reset renumbers every one of them, so the rows for a
  renumbered mailbox are now dropped at the reset: a fresh message landing on a
  reused UID gets its own three attempts instead of inheriting a stranger's
  give-up, and rows for UIDs the server stops listing are reclaimed.

- **The website no longer describes the file era (#0070, docs).** The published
  pages still showed a per-mailbox local directory tree, a `.md` plus companion
  `.html` per received message and a `_attachments/` sibling directory, none of
  which the binary has written since the store cutover. The data-directory
  layout, the first-sync walkthrough, the attachment and HTML answers in the FAQ
  and the draft-format attachments section now describe rows and blobs, and say
  which paths are still real: drafts, and the per-message directory a forward
  materialises its source's attachments into.

- **Contacts rebuilds are deterministic and no longer erode a populated corpus
  (#0067).** A message whose `Date:` header is absent or unparseable used to be
  stamped with the wall clock, so it floated to the top of its frecency tier and
  got a different value on every rebuild; it now gets a constant timestamp and
  sinks, the same rule the store's `date_sort = 0` marker applies to those rows.
  A rebuild that comes back with under a fifth of the cached corpus is refused
  as a partial read (the zero case was already refused, #0053), and both `mp
  contacts rebuild` and the TUI say how many they found against how many they
  kept. A corrupt `contacts-cache.json` no longer makes a rebuild fail: it warns
  and counts as empty, so rebuilding repairs it. A cache-save failure in the TUI
  refresh is no longer overwritten by the success status one line later. And a
  `default_from` written as `Name <addr>` now filters the user's own address out
  of their own contact list.

- **The mutation queue converges a crash-replay instead of surfacing a false
  failure (#0039 review, internal).** A move, delete or mark-read whose server
  half landed just before a crash used to replay against a message the source
  folder no longer held; the IMAP move/delete and Graph mark-read backends
  reported that not-found as a plain error, so the drain retried it to the
  budget, parked a succeeded op as `failed` and rolled the local state back
  under it. Those backends now return a typed `ops::NotFoundOnServer`, and the
  drain treats it as a converged replay (retire the row, keep the optimistic
  write). Direct CLI and TUI callers still see the same user-facing "not found"
  error, so deleting a message the server no longer holds still reports it.

### Changed
- **The post-send `\Answered` / `$Forwarded` write is off the send path and
  durable (#0076).** Flagging the source of a reply or a forward used to open a
  full IMAP session per mailbox the source was filed in, on the tail of a
  successful send: three logins for a message held in inbox, archive and sent,
  multiplied per draft by `mp send-approved`. It now commits the local flag and
  a single multi-mailbox `set_answered` op on the `pending_ops` queue in one
  transaction, and the background drain writes every folder over one IMAP
  session at the next sync or startup. The send path spends no network at all
  on bookkeeping, and a flag write that fails is retried instead of lost. The
  durability rules are unchanged: the enqueue happens after delivery and touches
  no outbox row, so bookkeeping still cannot fail, delay or re-send a message,
  and a refused flag op is never rolled back, because the reply did go out.
- **Archive, delete, move and flag toggles are now durable, in the TUI and the
  CLI alike (#0039).** Both frontends route every mutation through the
  `pending_ops` queue: the local change and the server op it owes commit in one
  transaction, so a crash between the two can no longer strand an optimistic
  move or lose a flag change. In the TUI the change is instant and the server op
  is retired in the background by the drain at the next sync or fetch (it builds
  no connection and adds no traffic when nothing is owed); a refusal is rolled
  back there and named in the sync line. This retires the per-mutation server
  threads and, with them, the "Quick sync queued (N ops pending)" stacking a
  sync used to log once per keypress. `mp archive` and `mp delete` keep their
  synchronous feel, enqueueing and then running the op in the same call, and
  still print the same not-found error for a message the server no longer holds.

- **The Contacts and Calendar views got a visual-polish pass (#TKT-0048).**
  They now share the Mail list's cursor-row convention (a raised surface fill
  carrying the selection foreground, in place of the solid green highlight each
  view had invented) and, in the widest layout, own the full frame the way Mail
  does: the ranked list fills the left column and the detail pane the right
  one. The mailbox sidebar and the blank left-middle slot that used to sit
  beside these views off the Mail view are gone; the view switcher stays pinned
  bottom-left. Purely cosmetic, no key bindings or data changed.

- **IMAP sync fetches its mailboxes in parallel now (#0005).** A sync used to
  SELECT each mailbox in turn on one connection, paying the round-trip latency
  once per mailbox; it now opens one connection per mailbox and overlaps them,
  turning `N * latency` into roughly one. On a warm three-mailbox account this
  cut `mp sync` from about 1.9s to 0.7s (Gmail) and 2.5s to 1.3s (Exchange).
  The fan-out is capped by a new per-account setting, `[accounts.imap]
  fetch_concurrency` (default 4, range 1-8); set it to 1 to restore the old
  single-connection behaviour if a server throttles. Only the network fetch
  runs in parallel, ingest still writes to the store serially in mailbox order,
  so nothing about the prune or flag behaviour changes.

- **The config directory is now `~/.config/mailypoppins/` (#0022).** It was
  `~/.config/email/`, which was the last user-visible place the old name
  survived. Your existing directory is moved for you, once, the next time you
  run `mp`, and the move is announced on stderr. Nothing inside it is read or
  rewritten, so passwords, signature files and account settings come across
  untouched. If the move cannot be done, `mp` says so and prints the exact `mv`
  to run rather than starting up against an empty config.

  One thing the move cannot fix for you: a value in `config.toml` that points
  *into* the old directory, such as a signature at
  `~/.config/email/signatures/work.html`. The file moved, the string in your
  config did not. `mp` names every such key, its old value and the exact
  replacement, once, right after the move, because otherwise the first you
  would hear of it is a message going out unsigned.

  Both `config.toml` and `secrets.enc` live there. A new
  `MAILYPOPPINS_CONFIG_DIR` env var overrides the location, mirroring
  `MAILYPOPPINS_DATA_DIR`; setting it skips the move entirely.

- **The OS keyring service is now `mailypoppins` (#0022).** Only relevant if
  you opted into `secrets_backend = "keyring"`. Nothing is lost: a password not
  found under the new name is looked up under the old `email-cli` one, and your
  next `mp config set-password` files it under the new name for good.

- **The Cargo package and library are now `mailypoppins` (#0022),** not `email`.
  Invisible unless you build from source, where imports read
  `use mailypoppins::...`. The installed binary is still `mp` and the version
  string is still `mailypoppins X.Y.Z`.

- **A sent draft records `sent_via: "mailypoppins X.Y.Z"` (#0022),** not
  `email-cli X.Y.Z`.

### Fixed
- **A draft whose frontmatter will not parse no longer vanishes silently
  (#0080).** A YAML mistake in `$EDITOR`, such as an `attachments:` list item
  written `-"/path"` without the dash-space, made the whole frontmatter
  unparseable; the drafts index skipped the file with only a log line, so the
  draft dropped out of the TUI Drafts list and `mp list` while sitting on
  disk. The skip is now surfaced. `mp list` prints a warning block after its
  listing naming each skipped file and its parse error (exit code unchanged),
  and the TUI shows the file in the Drafts list as an unopenable error row: a
  warning glyph, the filename, the theme's error colour, and the parse error
  in the preview pane. `Enter`/`e` on that row opens the raw file in `$EDITOR`
  so the YAML can be fixed, `d` deletes it, and send/approve decline cleanly.
  A bare or empty `attachments:` key was already tolerated and still is; no
  other malformation is auto-repaired.
- **A cross-account `mp://` selector now resolves against the account it names
  (#0073 follow-up).** Commands opened the store for `-A`/the default account
  before parsing the selector, so `mp delete mp://tum/drafts/<id>` under a
  different default searched the wrong account's index and failed with a
  right-looking `no match … of tum/drafts`, while `mp delete -A tum <same>`
  worked. Every selector command (delete, reply, forward, archive, save, open,
  path, edit, mark-approved, mark-draft, validate, preview) now resolves the
  account from the selector first, then opens the right store and, for received
  mail, loads the right server credentials. Commands whose transport is bound
  before the selector is parsed (`mp send`, `mp invite`) refuse a cross-account
  selector loudly ("selector names account X but this command is bound to Y")
  rather than acting on the wrong account.

### Added
- **See a message's whole conversation (#0008).** `T` on a message in the list
  opens a conversation overlay: every message that belongs to the same thread,
  oldest first, across every mailbox of the account (a reply sitting in Sent
  shows beside the original in Inbox). `j`/`k` move through it, `Enter` opens
  the highlighted message (switching mailbox when it lives in another one), and
  `Esc` closes. Each row carries its date, sender, mailbox and the same status
  glyph the list uses; a caret marks the message the overlay was opened from.
  The grouping is derived from the `In-Reply-To` / `References` headers ingest
  already resolves into a per-message `thread_id`, so no re-parsing or extra
  sync happens when the overlay opens, and a message whose relatives are not
  downloaded says so rather than opening a one-line overlay. `mp dump-mailbox
  --json` now carries a `thread` field beside `invite`, the conversation key,
  so a script can group related mail without touching headers; this closes the
  "list the related emails" half of #TKT-0051 that threading was always meant
  to own.

- **Flag important messages with a server-backed star (#0007).** `*` on a
  message in the list toggles the IMAP `\Flagged` system flag, marking it
  important the same way every other mail client does. A flagged row shows a
  coloured flag glyph before its subject, and the star is orthogonal to the
  read state, so a message can be flagged and still unread. The toggle is
  optimistic: the local store updates instantly and a background `UID STORE`
  mirrors it to the server, exactly like the read/unread toggle; if the server
  refuses, both halves roll back. It rides the same flag column and sync
  semantics as the read/answered/forwarded axis, so a star set or cleared in
  another client comes back on the next sync, and `mp dump-mailbox` reports
  `flagged` in its flags array. With a multi-select active, `*` flags the whole
  set, flagging when any is unflagged. On a Microsoft Graph account the star is
  kept locally but not mirrored, because that backend stays read-only-aware for
  now; a local filtered flagged view is deferred to #0079.

- **Delete a draft (#0073).** A draft could be created, edited, approved and
  sent, but never removed: `mp delete` only spoke the received-mail selector
  grammar, and the TUI `d` key reported "nothing to delete" on a Drafts row.
  `mp delete mp://<account>/drafts/<id>` now removes the draft file and its
  index row, and `d` on a Drafts row does the same behind the usual
  confirmation. Deleting is local-only, so there is no server round-trip; the
  same index rescan that heals a hand-deleted file drops the row.

  An approved draft is a queued send, so deleting it is refused unless you pass
  `--force` (or demote it first with `mp mark-draft`), and a draft an active
  outbox submission still holds is refused outright, on any flag, so a delete
  cannot pull a send's file out from under it (#0063). `mp delete --sent`
  clears every `status: sent` draft of an account in one call, which is the
  upgrade path for a directory of already-sent drafts left behind by an older
  build.

- **Read a received message in your editor again (#0075).** `Enter` or `e` on
  a message in any mailbox opens it in `$EDITOR`, as Markdown with the headers
  in YAML frontmatter and the body below them. This worked before every message
  moved into the store; the files went, and nothing replaced what they were good
  for, which is searching, folding, and copying out of a long thread with the
  tools you already have rather than through a preview pane.

  The copy is read-only and temporary. It is rendered from the store when you
  press the key, written mode 0444 so the editor opens the buffer read-only, and
  deleted when the editor exits. Nothing reads it back, so an edit forced past
  the read-only buffer reaches nothing, and the status line says so on the way
  out. The frontmatter carries `from`, `to`, `cc`, `subject`, `date`,
  `message_id`, the mailbox, the attachment names, and the whole second status
  axis (`read`, `answered`, `forwarded`).

  `Enter` and `e` on a Drafts row still open the draft file for editing, as
  before. In the IMAP search overlay the same key opens any hit that is already
  in the local store.

- **A second status axis: read, answered, forwarded (#TKT-0051).** A message
  used to have one bit of history, read or unread. It now carries three, and
  they are independent: the list's marker column shows a dot for unread, a
  reply arrow once you have answered it, a forward arrow once you have
  forwarded it, and nothing for a message you have only read. Unread wins over
  the two history glyphs, because new mail is what the list exists to surface.

  The state is never typed in. Sending a reply marks the message it answers,
  sending a forward marks the message it forwards, locally and on the server
  (`\Answered` and the `$Forwarded` keyword every other client already
  writes), and the reverse direction works too: a reply you sent from your
  phone shows up here on the next sync. The flag is written after the message
  has actually gone out, so a reply draft you abandon claims nothing. Reply and
  forward drafts record their source in a new `in_reply_to:` /
  `forwarded_from:` frontmatter key, which is what the post-send step reads.

  Nothing has to be re-downloaded for this: the axis lives in the flag column
  the store already had, and the next sync fills in the two new bits for
  everything in its window. `mp dump-mailbox` reports them as `answered` and
  `forwarded` beside `seen`. On a Microsoft Graph account only the read bit
  syncs, because Graph exposes the other two through extended MAPI properties;
  what mailypoppins sets there is kept locally and is not erased by a sync.

### Changed
- **A mailbox is identified by its role, not by a directory path (#0064).**
  The types still described a filesystem namespace the store cutover deleted: a
  sidebar mailbox carried a `PathBuf`, the role was a bare string compared
  case-insensitively in half a dozen places, and `EmailStatus` carried `inbox`
  and `archived` variants that were file placement states rather than draft
  states. There is now one `MailboxRole` type (`inbox`, `archive`, `sent`, or
  an unmapped mailbox holding its server name), the sidebar carries the store
  key itself, and `status:` in a draft is one of `draft`, `approved` or `sent`
  and nothing else. `mp sync --mailbox INBOX` files its rows under `inbox`,
  where the sidebar and the selectors look for them, instead of under a second
  `INBOX` key nothing lists.

  One real bug falls out of the merge. The sidebar built its store key by
  slugifying the server name of an `[[mailboxes.extra]]` mailbox while sync
  ingested that mailbox under the server name verbatim, so an extra mailbox
  named `Team/Reports` listed empty, counted zero, and swallowed any message
  quick-moved into it under a key sync never reads. Both sides now use the one
  key. For anyone with such a mailbox, the synced rows were the ones that could
  not be reached: they carry the server name, and the slug-shaped filter behind
  the sidebar, `mp dump-mailbox` and `--mailbox <slug>` never matched them, so
  the mailbox read as empty everywhere. What did carry the slug was a row a
  quick move had written under it, which is the half that has to be re-filed.
  The selectors and the dump keep spelling a synced row's mailbox segment with
  the server name, as they always did, and `--mailbox` now takes the server
  name or the sidebar label rather than the slug.

  A `.md` file whose frontmatter says `status: inbox` or `status: archived` no
  longer parses. Nothing has written one since the receive path stopped writing
  files, and no draft was ever created with one.
- **One send implementation behind `mp send`, `mp send-approved` and both TUI
  send keys (#0058).** Sending a draft was written out four times, each with
  its own copy of the recipient parsing, the attachment reading, the quoted
  HTML lookup, the SMTP-or-Graph choice and the post-send bookkeeping, so every
  durability fix had to be made four times and the copies had already drifted:
  the TUI's send-approved never refreshed the drafts index, `mp send-approved`
  over Graph skipped an attachment it could not read and sent the mail without
  it, and a Graph transport error there aborted the whole batch instead of
  counting one failure. `send::send_draft(&draft, &SendContext)` is now the
  single orchestration: it builds the bytes (which is where the approved-status
  requirement lives), commits the outbox row, submits over whichever transport
  the context names, and retires the draft file. The four call sites keep only
  what differs between them, the prompt, the wording and the exit code. Reply
  and forward draft creation was duplicated the same way and is now
  `draft::create_draft_from_source`, one build-rewrite-mint-reindex sequence for
  `mp reply`, `mp forward` and the TUI's `r` / `R` / `w`. Net 200 lines of
  duplication gone, with the two behaviour changes that follow from sharing one
  path: an attachment that cannot be read fails the send everywhere rather than
  being dropped in silence, and the contacts index is bumped on every send that
  reached a recipient, including one whose draft file could not be retired
  afterwards. That second one changes `mp send-approved` over both transports
  and the TUI's single-draft send key; `mp send` and the TUI's send-approved
  already bumped unconditionally. `mp send-approved` now also names the reason
  on the line that says every recipient failed.

  Four smaller changes ride along, each one narrowing a place where the outcome
  of a send was reported wrongly. A draft file that cannot be retired after the
  message has gone out no longer turns the TUI's send key into `Send failed`:
  the failure is a logged warning and the status line reports the send that
  actually happened. `mp send-approved` over Graph now finishes the batch and
  exits 0 with a sent/failed tally, where a transport error used to abort the
  remaining drafts and exit 1, which is what the SMTP loop always did.
  `mp reply` and `mp forward` now exit 1 without printing a selector when the
  drafts-index refresh fails after the draft file is written, instead of warning
  and printing a selector nothing can resolve yet; the id-collision notices on
  that path moved from stderr to the log, where the rest of that scan's warnings
  already were. And the TUI's attachment-read failure is worded
  `Failed to read attachment: <path>`, the wording the CLI already used.

### Fixed
- **An IMAP host that answers nothing at all is given up on after 30 seconds
  (#0076).** Opening a session had no connect timeout, so a host that swallows
  the SYN rather than refusing it, a dropped route or a firewall configured
  that way, held whatever asked for that session for the operating system's own
  timeout, over two minutes on Linux. A sync, the idle watcher and the flag
  write that follows a reply all sat on it. Only the TCP connect is bounded,
  and generously: a server that answers and then goes quiet fails its read on
  its own, and a slow link on a good day must not lose its connection to this.
- **A first sync no longer switches the removal prune off for the whole
  account (#0072 sweep review).** The arrival gate derives the line it holds a
  pass to from what the mailbox is known to have held, and an empty store knows
  nothing, so the first capped sync of a mailbox bigger than the download window
  persisted a mark of `0`: a line demanding that every message the server lists
  be in the store before any pass counts as complete, which a window of 50 or
  100 never reaches, and which could not rise again because the carried mark is
  combined with `min`. Since the prune needs *every* mailbox complete before it
  applies anything, one such mailbox held back removals across the whole
  account, and the schema v5 rebuild plus a capped default (`-n 50`, and 100 for
  the TUI's quick sync) put every store in exactly that state on the first sync
  of the previous build. First contact is not an arrivals situation: with no
  cursor row and no rows, everything the server lists is backlog, which is the
  distinction the gate is built on, so that pass now hands nothing to the next
  one. It still reports itself short, which costs one conservative pass and
  nothing more. The bulk-move deferral the mark exists for is unchanged, because
  a mailbox that has been synced before *has* a cursor: a mailbox emptied
  elsewhere and then bulk-moved into holds no rows at all, and the top its
  cursor recorded is what the 200 copies a 100-UID window could not take are
  still measured against. A store the previous build wrote has its marks of `0`
  cleared once, on the first open by this one; that sweep is stamped in `meta`
  and never runs again, because a mark of `0` stays the right answer for a
  mailbox that held no message at all when it was last synced.
- **A bulk move no longer loses its messages one sync after the gate deferred
  it, and store schema v5 (#0072 review).** The prune gate held a pass back
  when a message that had arrived above the mailbox's high-water mark was not
  ingested, and recomputed that mark from the store on every pass, so it
  protected exactly one. Move 300 messages into a mailbox whose quick sync
  downloads the last 100 UIDs: the first pass defers correctly, the second one
  stands on a mark its own ingest has just raised to the top of the folder,
  finds the 200 copies it never fetched sitting *below* it, declares itself
  complete and prunes the source rows of messages the store holds no copy of
  and a positional window will never go back for. The mark is now persisted in
  the sync cursor (`sync_cursors.arrival_mark`, hence the schema bump) and the
  next pass is held to the lower of carried and derived. It clears the moment a
  pass reaches through it, which any full sync does, and also when the missing
  messages stop being listed at all, so it cannot deadlock. `mp sync -n 0`,
  which computes a full removal set and downloads nothing, no longer reports
  itself as complete coverage and forces the gate open. As always there is no
  migrator: an existing store is dropped and rebuilt empty on first open, the
  outbox is carried across, and the next sync refills it from the server.

  One behaviour worth knowing rather than fixing rides along, now written down
  in the ticket, in `docs/lessons-learned.md` and on the site's FAQ: on Gmail,
  archiving removes the `INBOX` label and the copy in All Mail keeps its
  original low UID, so it is not an arrival, no gate can wait for it, and while
  the inbox row is pruned on the next quick sync the archived copy is re-filed
  only by a full sync. On servers that implement a move as copy-and-expunge
  (Exchange, Dovecot) one pass does both halves.
- **`mp sync --mailbox <name>` files an extra mailbox's messages under the key
  the rest of the product reads (#0064 review).** The role came from the string
  typed on the command line while the server name came from the configured
  mapping, so `--mailbox projects` against a mailbox configured as `Projects`
  selected the right folder on the server and ingested its messages under
  `projects`, a key the sidebar never lists and no selector resolves. Both
  halves of a sync target now come from one mapping, and a `--mailbox` name
  that matches no configured mailbox is an error that names the ones that do,
  rather than a sync into a key nothing reads. That strictness reaches one case
  beyond the extra mailboxes: `mp sync --mailbox inbox` on an account whose
  config has no `[mailboxes.inbox]` used to work by accident, because the name
  was passed to the server verbatim and most servers resolve `inbox` to `INBOX`,
  and now errors. Plain `mp sync` never synced that mailbox for such an account
  either, and the error names the mailboxes that are configured.
- **A rebuild salvages a long-lived outbox again (#0066 review).** Reading the
  rows a damaged page hid works by probing positions the listing never named,
  and the probe started at position 1 whatever the table held. An outbox that
  has drained and refilled for years keeps its live rows at positions far above
  that, so the probe budget was spent entirely on the empty range below the
  first row and recovered nothing. It now starts where the table says its rows
  start.
- **An RSVP reply queues the `from:` address it validated (#0063 review).**
  `build_draft_message` was fixed to carry the normalised address; its twin on
  the RSVP path still stored the raw one, so an account address like
  `Doe, Jane <jane@example.com>` built a reply that failed every submission it
  would ever get.
- **A message archived in another client now leaves the local inbox on the
  next sync (#0072).** The prune diffed the store's UIDs against the download
  window only, the last `--limit` UIDs the fetch had read, so a message the
  server had stopped listing was invisible to it as soon as it sat below that
  window: archiving the oldest mail elsewhere, which is what everyone does
  first, left rows that no number of quick syncs could ever remove. The
  enumeration was there all along, `UID SEARCH ALL` returns the whole mailbox
  and only the download is capped, so the diff now runs against it. One clamp
  survives, at `UIDNEXT - 1`, which is the line between a UID the server issued
  and a placeholder this client wrote for a Sent copy the server has not filed
  yet. Two conditions hold the prune back for the whole pass rather than risk a
  wrong deletion: a mailbox whose listing came back shorter than its own
  `EXISTS`, and a pass that did not ingest every message that arrived above
  what the store already held (a bulk move whose destination window could not
  hold every copy). Both are reported now instead of passing for a clean sync,
  and a full sync applies what a capped one held back. IMAP also runs the age
  guard that already protected a just-sent copy on the Graph side.
- **A recipient the server refused is now recorded, retried and reported,
  instead of vanishing with the status line (#0063).** SMTP runs once per
  recipient here, so a submission has one verdict per recipient; the outbox took
  the first 250 as "accepted" and threw the rest away, which meant a message one
  of its two recipients never got was filed as a clean success and nothing ever
  retried or named the recipient who was refused. Each verdict is now committed
  to the row's envelope: who took it, who was refused for good and why. A retry
  attempts only the recipients that are in neither list, so a recipient that
  answered 250 is never spoken to twice, including across `mp outbox retry`; a
  5xx stops that recipient rather than being retried forever; a recipient that
  gave no verdict at all still parks the whole row for a human. A message that
  went out to some of its recipients and not to the others reaches `done` and
  keeps the note, so it stays in `mp outbox list` as `partial` with the refused
  addresses named, and in the TUI's badge as `OUTBOX 1 (1 partial)`, until it is
  discarded. No schema change: the verdicts ride in the existing `envelope`
  column, and a row queued by an older build reads as "nothing recorded yet".
- **One draft is one submission, however many times send is pressed (#0063).**
  The TUI could send the same draft twice, because the cursor send and an
  approved batch it is by definition also in each reach the send path on their
  own thread, and every build mints a fresh `Message-ID`, so the second run
  looked like an unrelated message to both the outbox and the Sent dedup search
  a retry uses. The outbox row now carries the draft it was built from and
  refuses a draft that already has a row it owns; the send path holds a
  process-wide slot per draft for the length of the send. A `failed` or `done`
  row does not hold the gate, so a deliberate re-send after a human has looked
  still works.
- **A send that failed before the transport no longer parks itself as "may have
  been delivered" (#0063).** The exactly-once marker is committed immediately
  before the SMTP session opens, but an error raised on the way in (an
  unparseable envelope sender, a transport that would not build) propagated with
  the marker still set, so the next resume read it as a submission that died
  inside the conversation and parked a message that had provably never been
  sent. Both send paths now record that failure as the clean pre-submission one
  it is, which clears the marker and leaves the row submittable.
- **A `from:` with a comma in the display name no longer dead-ends a draft, and
  a busy outbox no longer lets a send past the gate (#0063 review).** Three
  fixes from the review of the above. A draft whose `from:` reads
  `Doe, Jane <jane@example.com>` was checked in its quoted form and then queued
  in its raw one, so it enqueued cleanly and then failed every submission it
  would ever get; because the queued row holds the gate and `mp outbox retry`
  refuses a row that has not been submitted, `mp outbox discard` was the only
  way out. The address that is checked is now the address that is queued.
  Second, two processes sending at the same moment could collide on the outbox
  in a way SQLite reports as a busy database, and that error was treated as "no
  store available", which sent the message with no outbox row and no duplicate
  gate at all; a busy outbox is now reported as a retryable error and only a
  store that will not open buys a send without a record. Third, two accounts
  each holding a hand-written draft with the same `id:` no longer refuse each
  other's send.
- **A store rebuild no longer discards queued mail or orphans its blob files
  (#0066).** `Store::open` answers an unusable store file, a schema version that
  moved, a failed `integrity_check`, a file that will not open as a database, by
  deleting it and creating an empty one, on the grounds that the store is a
  cache the next sync refills. That is true of every table but `outbox`, which
  is the record of what has been submitted to a mail server and which no sync
  can reconstruct: a message accepted by SMTP but not yet copied to Sent, or one
  queued for a retry, simply stopped existing, with nothing said to the user
  although `mp outbox list` presents those rows as durable send state. The v4
  bump (#0054) triggered that path for every account at once. Unfinished rows
  (`pending_send`, `sent_pending_append`, `failed`) are now read out of the old
  file before it goes, column by column and by name so an outbox of an older
  shape still comes across, and written into the new one with a reference on the
  raw RFC822 blob each one points at. `done` rows owe nothing and stay behind;
  a row whose state is unreadable is carried as `failed` rather than as
  something a driver would re-submit.

  The blob files were the other half: they survived the rebuild while the
  refcount rows did not, so everything the next sync did not fetch again became
  a permanent orphan that nothing reclaimed. The rebuild now sweeps the blob
  tree against the rebuilt `blobs` table, taking misplaced blobs and `.tmp`
  leftovers with it, and keeping exactly what the carried outbox rows point at.
  Anything that could not be carried is named in a `store-rebuild-<timestamp>.txt`
  note next to the store and in the log line the rebuild already emitted, which
  now counts what was carried, what was discarded and how many blob files were
  swept.

  The review of that change found the salvage reading the old outbox in one
  scan, which a damaged page ends for good: SQLite resets the statement on a
  step error, so every later read answers "no more rows" and the whole tail of
  the table disappeared with the note reporting nothing discarded (196 of 400
  rows carried, 204 unnamed, on the probe). The salvage now reads row by row,
  addressing each row by position and then going back for every position the
  listing itself never named, so a damaged page costs the rows it holds rather
  than every row behind it (372 of the same 400), and whatever is still
  unreachable is counted and named in the note. A submission marker that was written but
  cannot be read as a timestamp now parks its row as `failed` instead of
  salvaging as empty, which the send path would have read as "never submitted"
  and handed back to SMTP. The blob sweep no longer walks a symlinked `blobs/`
  root, where it could have deleted the store file it had just rebuilt. A
  salvage reads at most 10 000 rows and says so when the table held more, and the
  note file's timestamp carries milliseconds so two rebuilds in one second
  leave two notes.
- **A failed sync is now visible without reading the log, per account and per
  exit code (#0071).** #0068's account-level `error!` line put the failure in
  the log file; it was still invisible on screen, because the outcome of a sync
  only ever existed as one shared status line and the last writer won. The last
  writer is whichever account is slowest, which is never the one that failed
  fast: `perso` failed after 54 ms and `tum` overwrote the line with
  `Fetch complete` fifteen seconds later, every tick, for seven weeks. Each
  account now carries its own `SyncHealth` (`Unknown` / `Ok` / `Failed` with the
  reason, the time and a consecutive-failure count), written when that account's
  own result lands, so a failure survives every later success of a different
  account and clears only when that same account syncs cleanly. Every sync path
  writes it: the startup multi-account fetch, the watcher-triggered quick sync,
  `F`, `S`, IMAP and Graph alike. The TUI reads it in two places that no status
  line can race away, a three-row block under the sidebar's mailbox list
  (`⚠ sync failed x12 15:42` and the wrapped reason) and a `⚠` marker on the
  failing account in the status-bar account strip, next to the unseen badge. The
  status line that reports the failure now names its account too.

  `mp sync` gained `--all-accounts`, a loop over exactly the single-account
  body: one account's failure no longer stops the others, every failure is named
  on its own line and again in a closing `1 of 3 account(s) failed to sync:
  perso`, and the run exits 1. One code for any failure, partial or total, and
  the same code for `--all-accounts` as for a single named account, because the
  caller writing `mp sync --all-accounts || alert` is the reader this exists
  for; a distinct partial code is only legible to a caller that already knows
  how many accounts are configured. The single-account form already exited
  non-zero and still does.

  Three review follow-ups ride along. `mp sync --all-accounts` no longer counts
  a local-only account as a failure: an account with no IMAP host and no Graph
  config is the drafts-only case the TUI already supports, so the run prints
  `- <name>: local-only, skipped` and leaves the exit code alone, where it used
  to exit 1 on every run of a config that legitimately holds one.
  `--all-accounts` and `-A/--account` now conflict instead of the selector being
  ignored in silence. An `mp sync` with no account to sync says so instead of
  reporting `✗ : <error>` for the empty default account.
- **An account whose sync fails now says so in the log (#0068).** A sync that
  died at the account level, a refused IMAP login above all, produced one
  transient TUI status line and nothing else: in a multi-account sync the
  accounts that succeeded overwrote that line seconds later, so a Proton Bridge
  that had been signed out since June went unnoticed for seven weeks while
  roughly 2900 logins were refused. The per-mailbox failure right below it had
  warned all along; the account-level one now logs at error level too. The
  persistent surface this also wants, a per-account health mark in the TUI and
  an `mp sync` that exits non-zero naming the failed accounts, is #0071, above.
- **A Graph account with an unloadable Graph config no longer sends over SMTP
  (#0058 follow-up).** Both TUI send keys chose their transport by asking
  whether a `GraphConfig` had loaded, but `AccountState` loads that config
  best-effort, so a Graph account whose config failed to load fell through to
  whatever SMTP config happened to be configured and sent the mail from there,
  under an identity Graph would have stamped. The choice is now the account's
  `auth_method` alone, which makes that case the `Graph not configured` error it
  was always meant to be, and makes the status reachable.
- **The Graph prune no longer deletes the copy of a message you just sent
  (#0065).** Sending through Graph files the local copy under a uid derived
  from our own `Message-ID`, but `sendMail` transmits JSON and Exchange stamps
  an id of its own, so the Sent folder never lists the message under the
  identity the store gave it: the prune #0055 added saw an orphan and deleted
  it, releasing the raw MIME with it, which for a Graph account is the only
  copy there will ever be. A row dated within the watcher's longest poll
  interval is now left alone, which carries the copy through the window where
  the server has not filed the item yet without making it immortal: once the
  server's own copy is in the store, the later pass still clears the duplicate.
  Five more hardenings on the same two functions. The folder enumeration keys
  on the trimmed `internetMessageId`, matching what ingest stores, so a padded
  header can no longer make every message look new *and* vanished at once, a
  delete-and-re-download loop every sync. The enumeration walks the folder
  newest-first and gives up after 250 pages, so a message can neither be shifted
  out of an unordered page window by a concurrent arrival nor silently dropped
  by an endless `nextLink` chain. **A capped quick sync no longer prunes at
  all**: with `-n 100` and a larger backlog, the inbox rows of a hundred
  messages moved to Archive at once used to go in the pass that had not yet
  downloaded their archive copies, leaving them with no row anywhere until the
  backlog drained; the prune now waits for a pass that saw every message in
  every mailbox. Batch sub-request ids are percent-encoded, and a throttled
  sub-response's `Retry-After` paces the rest of the pass rather than being
  ignored, with a give-up after fifty failures so a systematically failing first
  sync costs one pass and six log lines instead of one request and one warning
  per message.

  A follow-up review of that work closed the hole left in the middle of it. The
  prune gate asked only whether `limit` had capped the download, so a mailbox
  whose messages were throttled or failed inside the batch reported a complete
  pass while holding none of them: a sync whose Archive fetch came back empty
  could still prune the inbox rows of messages that had moved there, which is
  the exact loss the gate was built to prevent. A pass now counts every way it
  can come up short, a message asked for and not returned and a message
  downloaded but not written included. Three smaller ones alongside it. A
  sub-response header that is a number rather than a string no longer fails the
  parse of its whole batch, which would have meant zero downloads for that
  folder on every pass with the prune suspended throughout. If a tenant rejects
  the newest-first enumeration, the folder is re-walked unordered instead of
  never syncing again. Throttling no longer spends the failure budget meant for
  requests that cannot succeed, a 503 carrying a `Retry-After` is read as the
  throttle it is, and the pause it asks for is taken before the next chunk goes
  out rather than after the last one, where it delayed the sync for nothing.
- **A Graph sync now converges, prunes, and says so in the timing log
  (#0055).** The sync fixes the IMAP path got over the last months had never
  reached the Graph backend, which still behaved like the file era. Six defects
  went together. **Mail archived or deleted in Outlook web now disappears
  locally**: the folder enumeration already covered every message, so the ids
  the store holds and the server no longer lists are the vanished set, pruned
  after every mailbox has been ingested, the same ordering the IMAP side uses so
  an archived message always has its archive row before its inbox row goes. **A
  message that is new to the store but old on the server is downloaded once**
  instead of being re-detected on every sync forever: detection looked at the
  whole folder while the download asked for the most recent `$top` messages, a
  mismatch a `skipped.min(20)` fudge admitted to; messages are now fetched by
  their own id, twenty per Graph `/$batch` call, newest first so a capped pass
  still takes the arrivals a user is waiting for. **The TUI watcher compares the
  set of inbox ids rather than how many there are**, so one arrival plus one
  archive inside the same minute is no longer invisible, and it keeps one client
  instead of building a fresh one every poll, which saves the connection pool
  and the network refresh of an unexpired token (the cached token blob is still
  read and decrypted on every pass). **A revoked Graph token now reaches the
  user**: it used to be one silent failed request per minute forever, and is now
  a widening poll interval sharing the outbox's backoff curve, up to 15 minutes,
  plus a visible watch error after three consecutive failures. A sync's
  per-message read-flag updates land in **one transaction per mailbox** on both backends
  rather than one commit per message, and the Graph sync path carries the same
  `[TIMING]` marks as the IMAP one.
- **A contacts rebuild no longer wipes the frecency index (#0053).** The
  extractor was the last thing still walking the `.md` tree the store cutover
  deleted: it found zero messages, and `mp contacts rebuild`, the cold-cache
  build and the TUI refresh key each cached that nothing over a corpus months
  of use had accumulated. The rebuild now reads the same `messages` rows the
  TUI and `mp dump-mailbox` read, taking the from/to/cc headers and the
  observation role straight off the row (its `mailbox` column), so it produces
  the index the mail actually supports: 1733 contacts on the largest live
  account, top ten unchanged against the pre-rebuild corpus. Two guards sit
  behind it: **an empty rebuild never replaces a populated cache**, it says so
  on the console and in the log and leaves both the file and the TUI's loaded
  index alone. That is what an account whose store holds no rows yet now does
  instead of losing what the send/sync hooks had collected for it.
- **The Body pane fills for a draft row.** Selecting a draft in the TUI showed
  its headers and an empty body: the preview's memo was keyed on the store row
  a draft does not have, so the key never built and the pane was never filled.
  The memo is now keyed on the entry, message or draft, and a draft row reads
  its markdown from the file the drafts index points at. The lookup is a plain
  read on the UI thread, never a re-index. A row whose file has gone from under
  the index previews empty rather than failing the frame.
- **A sent draft leaves `drafts/`.** Sending rewrote the draft's `status:` to
  `sent` and left the file where it was, so a message that was already gone sat
  in the TUI's Drafts list and in `mp list` forever with nothing left to do to
  it. A send that reached **every** recipient and got an outbox row now deletes
  the draft file: the copy that matters lives on the server, which the durable
  outbox APPENDs to Sent and ingest reads back into the store. A **partial**
  send keeps the file, marked `sent`, because it is the only thing that names
  the recipients who did not get it, and so does a send the outbox store could
  not record, whose only local copy the file is. Sending such a draft again is
  a hand edit rather than a command: `mp send` only builds an `approved` draft
  and neither `mark-approved` nor `mark-draft` will touch a file that says
  `status: sent`, so the user edits `status:` back to `approved` themselves,
  after trimming the recipient lines, because a re-send delivers to everyone
  the file still lists including the ones who already received it. Applies to
  `mp send`, `mp send-approved` and both of their TUI equivalents, over SMTP and
  over Graph. A file hand-edited to `status: sent` is still listed, unchanged.
- **Sync prunes the rows the server no longer lists (#0038 follow-up).** A
  message archived, moved or deleted in another client kept its local row
  forever, and because ingest identity is per mailbox the same message was
  inserted again when the archive synced: the user saw it in two mailboxes at
  once. Sync now compares what the store holds against what the server listed
  and **deletes the local rows the server no longer lists in that mailbox**,
  clamped to the numeric range the fetch window actually covered, so a short
  `-n 50` window can only ever prune inside what the server proved and a
  UIDVALIDITY reset prunes nothing. The prunes are applied after every target
  mailbox has been ingested, so a message that merely moved is already present
  at its destination before its source row goes: it is never absent from the
  store, and its body blob is never released or deleted from disk in between.
  The count is reported by `mp sync` and in the TUI status line. The Graph
  backend still has the same gap and is untouched.
- **The list reloads after a fetch or a sync (#0038 follow-up).** Both only set
  a status line, on a comment that stopped being true when the read path moved
  onto the store: the user pressed refresh and nothing refreshed until a mailbox
  switch or a restart. Every per-mailbox cache of the synced account is now
  dropped, the open mailbox is reloaded off the UI thread the way a mailbox
  switch reloads it, and the sidebar counts are recomputed.
- **The queued-sync status line stops stacking.** One `s` on a busy account
  produced roughly four "Quick sync queued" activity lines a second for as long
  as the background work ran: the event loop released the parked action on one
  condition and the action re-entered a gate reading another. Release and gate
  are now the same condition, and re-parking the same action is silent.
- **Keypress lag and slow startup from a full-file `integrity_check`.** The
  check walks every page of the store (240 ms on a 44 MB file) and ran on every
  open, which the TUI does per call: once per keypress on the preview path and
  ten times before the first paint. It now runs once per file per process. The
  first open still validates in full and still triggers the drop-and-rebuild on
  failure, a rebuilt file is validated on its own next open rather than
  inheriting the dead one's verdict, and a file that fails is walked again
  rather than remembered as checked.

### Removed
- **The dead code the store cutover orphaned, and the last three strings that
  still described the file era (#0057).** Nine items whose callers went with
  the `.md` tree: the `select_inbox_email` draft picker that walked an inbox
  directory, the four `.md`-path helpers in `parse.rs`
  (`attachments_dir_for`, `account_dir_for_email`, `list_attachments`,
  `parse_email_date_prefix`), the `SaveFrontmatter` struct that described how a
  fetched email was written to disk, `config::resolve_mailbox_dir`, and the
  write-only `sent_dir` / `archive_dir` / `inbox_dir` fields carried on every
  account's TUI state. The fourteen tests that existed only to exercise them
  went too, which is the whole of the 820 to 806 suite delta. `open_store`, the
  opener that returns `None` rather than creating a store for an account that
  has never synced, moved from `tui::app` to `store` beside `Store::open`, so
  the contacts extractor and the dump path no longer reach into the TUI module
  for it. **`mp config show` stops printing mailbox directories that do not
  exist**: `[local paths]` now names the store, the blob directory and the
  drafts directory, and `[mailboxes]` maps each role to its server folder and
  nothing else. The `src/graph.rs` module comment no longer claims the client
  integrates with a local `.md` + `.html` + `_attachments/` layer, and `mp
  --help` now describes the whole product rather than the drafts half of it.

### Changed
- **The documentation and the two outputs that still described the file era
  (#0056).** `docs/architecture.md` is the file every session is told to read
  first, and it still taught "emails are files": a `.md` per message with an
  `.html` companion, a module map naming files that were deleted with the
  cutover, an in-memory Message-ID index that no longer exists and a test count
  from three hundred tests ago. It has been rewritten against the tree that
  exists: the store as a cache in front of the server and its drop-and-rebuild
  contract, the blob store, the ingest, read, mutate and send paths, both sync
  backends with their shared prune ordering, the selector grammar, and the TUI
  layering as it really is (`ui/` renders from state alone, `app/` opens the
  store synchronously, the protocol boundary is the part that is absolute).
  Two user-facing strings advertised the same retired model and are now true:
  `mp dump-mailbox` said it reads the local `.md` files, and `mp contacts
  rebuild` said it reads local mailbox files. The `config init` and
  `config add-account` wizards printed an Inbox, a Sent and an Archive
  directory per account that nothing has created since the cutover; all four
  completion blocks now print the account directory, which the wizard creates,
  plus the store, blob and drafts paths that go inside it and what makes each
  of them.
- **Store schema v4: the sync cursor stops storing a UID as a modification
  sequence (#0054).** `sync_cursors.highest_modseq` held the highest UID a
  fetch had seen, which nothing read as a modseq yet. It is now split in two:
  `last_uid` holds the highest UID a fetch saw and is what #0041 will resume
  from, and `highest_modseq` only ever holds a CONDSTORE modification sequence,
  staying NULL until #0041 issues `CHANGEDSINCE`. A UID-sized number passed as
  a modseq makes the server return nothing **and no error**, so the trap would
  have been silent. Three smaller corrections ride the same version bump:
  `sync_cursors` and `pending_ops` now carry the `account` column the rest of the schema carries
  (the cursor row is keyed `(account, mailbox)` like every other per-mailbox
  row), `pending_ops` gains the `updated` timestamp the #0039 backoff is a
  function of, and the two write-only columns (`messages.mtime`,
  `mailboxes.unread_count`) are gone. As always there is no migrator: an
  existing store is dropped and rebuilt empty on first open, and the next sync
  refills it from the server.
- **View switching moved to a `Space` leader; list toggle-select moved to `v`
  (#0033 follow-up).** The TUI view switcher is now **`Space m`** (Mail),
  **`Space c`** (Contacts), and **`Space a`** (Calendar) — replacing the earlier
  `g m/c/a`. Pressing **`Space`** arms the leader from every view and every Mail
  pane and shows the `m/c/a` continuations in the hint bar. To free `Space`,
  **toggle email selection** in the list moved from `Space` to **`v`** (`Esc`
  clear-selection and `Ctrl+a` select-all are unchanged). `g` stays a
  list-scoped leader for `gg`/`G` only.

### Added
- **The read path, calendar and reconcile run off the SQLite store; cold start
  stops walking files (#0038).** The TUI list, body pane, counts, calendar and
  iMIP reconcile all read `store.sqlite3` and the content-addressed blob store
  instead of a `.md` tree; sync ingests raw RFC822 keyed by UID in two passes
  (flags over the window, bodies only for new UIDs). `mp dump-mailbox --json`
  dumps store truth, verified live against the pre-nuke oracle captures with
  every difference classified in `docs/dump-allow-list.md`.
- **Every TUI mutation runs off the store and the selector (#0052).** Reply,
  Reply-all, Forward, Send, Approve, Mark-draft, their batch forms, Edit
  recipients, `$EDITOR` on a draft, attachment open and save, Open in browser
  and the calendar's Open event source all work again instead of declining with
  a status line, and each takes the same path its `mp` counterpart takes: the
  quote and the HTML companion come from the message's blobs, drafts are found
  through the drafts index, sends go through the durable outbox, and
  attachments are materialised out of `message_blobs` where `mp open` and
  `mp save` put them. The server-search overlay gets the same treatment on both
  halves of a hit: one that resolved to a local message reads the store, one
  that never synced is served from the fetch the overlay is already showing.
  Two flows decline permanently and say why: `$EDITOR` on a received message
  and Open on a search hit, both of which used to open a `.md` file that no
  longer exists. The temp directories those files are materialised into are
  created private to the user (0700) and refused if something else already
  holds the path, so a predictable name under a shared `/tmp` cannot be used to
  intercept message bytes; and `A` / `D` over a received-mail selection now say
  the selection holds no draft instead of asking to approve N drafts and then
  approving none.
- **`mp://` selectors and the drafts index (#0050).** Every command that names a
  message now takes a selector, `mp://<account>/<mailbox>/<key>`, and never a
  file path. The key is the Message-ID without angle brackets for received mail
  and the draft's `id:` frontmatter field for drafts, so renaming a draft file
  keeps its selector working. Leading segments can be elided (`mp send <id>`),
  a key that matches two mailboxes is reported with both full selectors instead
  of being resolved by guesswork, and `--mailbox` picks one. `mp path` and
  `mp edit` are the only edges back to the filesystem. `mp list` reads the new
  drafts index, `mp send-approved` takes `--all-accounts`, and `mp new`,
  `mp reply` and `mp forward` print the selector of the draft they created. In
  the TUI, the Drafts mailbox lists from the index (it was empty since #0038),
  a draft written by another process shows up within a second without a
  restart (closes TKT-0045), and `y` copies the selector instead of a file path.

### Fixed
- **Selector keys ending in `.md`, quoted HTML in replies, duplicate draft ids
  and the drafts count (#0050 review).** The filesystem-path heuristic now runs
  only on unqualified input, so a Message-ID on a `.md` ccTLD and a draft id
  ending `.md` survive their own canonical form. `mp reply` and `mp forward`
  quote the sender's HTML again, read from the message's html blob or its raw
  message. Two draft files carrying one `id:` still collapse to one index row,
  but the reindex now picks a deterministic winner and names both paths instead
  of losing one silently. The TUI sidebar counts drafts through the same read
  the Drafts list uses, so an account that has never synced no longer lists
  drafts and counts zero.

### Added
- **Durable outbox for sent mail (#0037).** Every outgoing message is committed
  to the per-account store (raw bytes plus an `outbox` row) *before* SMTP runs,
  and the transition to `sent_pending_append` is committed as soon as the server
  returns 250. SMTP runs exactly once per row: an ambiguous failure parks the
  row as `failed` for inspection and is never re-sent automatically, while a
  clean pre-submission failure stays sendable. Retry drives only the Sent
  `APPEND`, deduplicating through `UID SEARCH HEADER MESSAGE-ID` so a lost
  acknowledgement cannot produce a second copy, and resumes on startup and on
  every sync tick with backoff. A new per-account `save_to_sent` flag
  (`auto` by default, `always` / `never` to override) skips the `APPEND` for
  Gmail, Microsoft Graph and Proton accounts, whose servers file the copy
  themselves. Non-`done` rows show as an `OUTBOX n` badge in the TUI status bar,
  and `mp send` reports honestly: *sent + saved*, *sent + append pending*, or
  *failed*. A crash before the SMTP session opens no longer strands the
  message: the row records the moment submission starts, so the next startup or
  sync sends the ones that provably never reached the transport and parks the
  ones that died inside it. `mp outbox list` shows every queued, retrying and
  failed submission, `mp outbox retry <id>` sends a failed one again after you
  have checked it did not arrive, and `mp outbox discard <id>` drops it and
  releases its bytes.
- **UIDVALIDITY reset detection on sync (#0037).** A server that renumbers a
  mailbox hands the same low UIDs to different messages. The sync now compares
  the server's `UIDVALIDITY` against the stored cursor and refetches the whole
  window when they differ, instead of skipping bodies it has never seen;
  messages that only moved are rebound through their `Message-ID`, keeping
  their thread assignment and stored blobs.

### Removed
- **The local sent `.md` copy.** `update_status_to_sent` is now
  `mark_draft_sent`: it marks the draft in place and no longer writes or moves a
  file into `sent/`. The Sent copy lives on the server and in the store, put
  there by the outbox (#0037).

### Added
- **Search by Message-ID (TECHLEV-6).** `mp search` accepts a
  `message-id:` prefix that resolves an RFC 5322 Message-ID to its message:
  `mp search 'message-id:<abc@example.com>'`. Angle brackets are optional on
  input and always added on the wire, and the match is exact rather than a
  substring, so `<abc@x>` never returns `<prefix-abc@x>`. Works on both
  backends (IMAP `HEADER "Message-ID"`, Graph `internetMessageId eq`), across
  all configured mailboxes by default, and combines with the other prefixes
  (`in:Archive message-id:...`). The TUI server-search overlay (`f`) shares the
  same parser and gets it for free.
- **Local calendar view in the TUI (#0034).** Switch to Calendar with
  **`Space a`** for an **agenda over the invitations already on disk**: date,
  title, and your RSVP state per row, with the shared **event card** (time,
  location, organizer, your RSVP, per-attendee statuses, recurrence) in the
  detail pane. Navigate with **`j`/`k`** (and `gg`/`G`), **`Enter`/`e`** opens
  the invite email in `$EDITOR`, **`V`** RSVPs to a received invitation,
  **`t`** toggles between upcoming-only and all events, and **`r`** re-reads
  the account from disk. One row per event: the Sent/Inbox/Archive copies of an
  invitation collapse by iCal UID (highest `SEQUENCE` wins), RSVP replies are
  not listed as separate events, and an invitation with a matching `CANCEL`
  message is struck through and tagged `cancelled` (and refuses `V`, since the
  organizer already called it off). `.md` files that arrived as *email
  attachments* are never agenda rows, so a crafted attachment cannot spoof,
  displace or cancel a real invitation. The agenda is per-account and loads on
  first switch.
  **Caveat, stated in the pane itself:** this calendar is built *only* from
  invitation emails, so **events you created directly in Outlook (never emailed
  to you) are not shown** — they need the Graph sync backend (#0036). RSVPs
  are still emailed to the organizer and are not written to your Exchange
  calendar.
- **Contacts view in the TUI (#0033).** Switch to Contacts with **`Space c`** for
  a read-only, herdr-style **list + fuzzy search + detail pane** over your
  local contacts index (the same index that backs compose autocomplete). Press
  **`/`** to incrementally fuzzy-filter the list, **`j`/`k`** (and `gg`/`G`) to
  navigate, and the detail pane shows the selected contact's name, address, and
  interaction stats. From a contact you can **compose to it** (**`Enter`** or
  **`n`** opens the compose wizard seeded with the recipient — the overlay
  floats over Contacts, so you return there on submit/cancel), **send it as a
  vCard** (**`v`** exports the contact to a `.vcf` and starts a new draft with
  it attached), or **refresh the index** (**`r`** rebuilds it for the active
  account). The index loads lazily from cache on first switch; the off-Mail
  hint bar now advertises only keys that actually fire.
- **Multi-view TUI foundation: view switcher (#0033).** The TUI is now a
  multi-view client. A herdr-style bottom-left **view switcher** (`mail |
  contacts | calendar` chips, active one highlighted) sits under the mailbox
  sidebar, and the new `Space` leader combos **`Space m`** (Mail),
  **`Space c`** (Contacts), and **`Space a`** (Calendar) switch between them
  from any pane and any view. Mail is the full email client you already know; Contacts and
  Calendar render clean placeholder panes for now (Contacts content and the
  local calendar land in follow-ups). Mail-specific keys are gated to the Mail
  view — only view switching, quit, help (`?`), and the activity log stay live
  in the placeholder views — while **digits 1–9 still jump mailboxes** in Mail
  with no leader collision. Internally, the mail-specific `App` state is carved
  into a `MailView` sub-struct mirroring the existing account-state proxy, so
  the switch parks and restores per-view state cleanly. No existing key binding
  changed.
- **TUI mode/hint bar + `mp dump-keys` (keymap-as-data, #0032).** The TUI now
  shows a herdr-style hint bar above the status line: an accent-background mode
  badge (or `N SELECTED` when a selection is active) plus the next valid
  keystrokes for the focused pane, all derived from a single `KEYMAP` table. The
  previously-invisible `g` leader is now a first-class prefix chord — pressing
  `g` shows its continuations (`gg`, `G`) in the hint bar. `mp dump-keys` prints
  the key bindings as Markdown (or `--json` for tooling); the website key table
  is regenerated from it (`scripts/regen-website-keys.sh`), so the help overlay
  (`?`), the hint bar, and mailypoppins.dev all derive from one source and can
  no longer drift. Subsumes and closes #0019 (configurable keybindings
  groundwork). No existing key binding changed.
- **Organizer-side RSVP reconciliation (iMIP REPLY): `mp calendar rebuild`.**
  When an attendee accepts/declines a `mp send --invite` invitation, their
  mail client sends a `METHOD:REPLY` email that arrives via normal sync
  (parsed + saved with an `event:` block since #0027). Reconciliation
  matches each REPLY by event `UID` to your locally-stored sent invite and
  updates that invite's `event.attendees[].status` for the replying
  address (matched case-insensitively), so you can see who responded.
  `SEQUENCE` is respected — replies for a sequence older than the stored
  invite's are ignored — and when several replies exist for the same
  attendee+UID the one with the highest `(SEQUENCE, DTSTAMP)` wins. Runs
  automatically after each sync (IMAP and Graph), but only when the sync
  actually saved a REPLY, so the common no-calendar path stays free of a
  mailstore walk. The frontmatter update is a line-surgical rewrite of the
  matching attendee's `status:` line only (never a serde re-serialize of
  the whole file), so block scalars, interior blank lines, CRLF endings,
  and quoted/unicode attendee addresses round-trip byte-for-byte.
  Reconciliation is **idempotent and re-runnable over the whole
  mailstore**: `event.attendees[]` is local derived state reconstructible
  from IMAP-visible messages alone (the sent invite + the REPLY emails),
  so two machines syncing the same account converge on identical state
  with no machine-to-machine sync. `mp calendar rebuild [--account NAME]`
  recomputes every invite's attendee statuses from scratch (defaults to
  all accounts), mirroring `mp contacts rebuild`. A reply from an address
  that was never invited is ignored (the organizer's attendee list is the
  invite's). **Honest caveat:** this updates only the *local* mirror —
  without Graph the user's server-side Exchange calendar is never touched.
  `METHOD:CANCEL` and `SEQUENCE`-bump event updates remain out of scope
  (#0031). Ticket #0030.
- **RSVP to calendar invitations (iMIP REPLY): `mp invite accept|tentative|decline <email-path>` + TUI `V`.**
  Respond to a received invite (an email with an `event:` block and a
  sidecar `invite.ics`). Builds a `METHOD:REPLY` `VCALENDAR` whose `UID`
  and `SEQUENCE` are copied verbatim from the sidecar `.ics` (the source
  of truth, never the frontmatter cache), with a single `ATTENDEE` = your
  account address carrying the chosen `PARTSTAT`
  (`ACCEPTED`/`TENTATIVE`/`DECLINED`), a fresh `DTSTAMP`, and the echoed
  `ORGANIZER`. `RECURRENCE-ID` is intentionally absent — v1 answers the
  whole series only. The reply is emailed to the `ORGANIZER` as
  `multipart/alternative [ text/plain, text/calendar; method=REPLY ]`
  with an Outlook-convention subject (`Accepted: <summary>` /
  `Tentative: <summary>` / `Declined: <summary>`), and appended to the
  server Sent folder best-effort. On a successful send the local
  `event.rsvp` frontmatter is flipped in place (the sidecar is untouched)
  via the same safe in-place YAML rewrite machinery as draft edits. The
  TUI gains an invite badge (calendar glyph) in the email-list row, an
  event summary card at the top of the preview pane (title, time,
  location, organizer, your RSVP state, per-attendee statuses,
  recurrence, and the honest "not synced to Exchange" caveat), and a
  single `V` key that opens a small Accept / Tentative / Decline overlay
  (Esc cancels); the reply is sent on a background thread. RSVP is only
  offered for received `REQUEST` invites — your own sent invites make you
  the organizer and are guarded with a hint. Graph accounts error clearly
  (Graph RSVP is #0036). No RSVP note text and no per-occurrence
  responses in v1. Ticket #0029.
- **Send calendar invitations (iMIP): `mp send --invite`.** Send a
  `METHOD:REQUEST` invitation over SMTP that renders as an actionable
  Accept / Tentative / Decline event in Outlook, Gmail, and Apple Mail.
  New flags: `--invite --to <addr> [--cc <addr>] --subject <text>
  --start <when> (--end <when> | --duration <dur>) [--location <text>]
  [--description <text>]`. The subject is reused as the event `SUMMARY`
  and attendees come from `--to`/`--cc` (each an `ATTENDEE` with
  `RSVP=TRUE`). `--start`/`--end` accept local wall-clock time
  (`2026-07-20T14:00`, `2026-07-20 14:00`) or RFC3339 with an offset
  (`2026-07-20T14:00:00+02:00`, `...Z`); `--duration` accepts ISO8601
  (`PT1H30M`) or short form (`1h30m`). The message is built as
  `multipart/mixed [ multipart/alternative(text/plain, text/html,
  text/calendar; method=REQUEST), application/ics ]` — the inline
  `text/calendar` part is the contract and the `application/ics`
  attachment is optional hardening. `ORGANIZER` is set to (and validated
  against) the sending account's primary address, since Exchange drops
  mismatched invites. The event `UID` is a collision-resistant
  `sha256(now + randomness + organizer)` hex prefix scoped to the
  sender's domain (no new dependency). The sent invite is persisted
  locally as an email `.md` with an `event:` block plus a sidecar
  `invite.ics`, so it round-trips through the receive path (#0027) and
  anchors reply reconciliation (#0030). Graph-auth accounts error clearly
  (Graph calendar send is #0036). RSVP replies, cancellations, recurring
  creation, and TUI compose integration are out of scope. Ticket #0028.
- **Receive calendar invitations (iMIP): detect, save, and parse
  `text/calendar` parts.** Incoming iMIP invites (Outlook, Google, Apple)
  were previously silently dropped by the MIME part traversal. Fetching
  now detects any `text/calendar` part (inline or `.ics` attachment, on
  both the IMAP and Graph paths), saves the raw payload as a sidecar
  `invite.ics` in the email's `_attachments/` directory (source of truth
  for `UID`/`SEQUENCE`), and parses the first `VEVENT` into a nested
  `event:` frontmatter block (`uid`, `method`, `sequence`, `summary`,
  timezone-aware `start`/`end` as RFC3339, `location`, `organizer`,
  `attendees` with per-attendee `status`, own `rsvp` status initialized
  to `needs-action`, and a human-readable `recurrence` summary for
  `RRULE`). `REQUEST`/`REPLY`/`CANCEL` methods are all detected and
  stored (semantic handling of REPLY/CANCEL is deferred). Parsing is
  best-effort: a malformed `.ics` still saves the sidecar and the email,
  just without an `event:` block. Emails without a calendar part are
  stored exactly as before. Ticket #0027.
- **TUI: edit a draft's recipients/subject with the fuzzy compose wizard
  (`c`).** In the Drafts mailbox, press `c` on a draft to reopen the
  compose wizard pre-seeded from the draft's existing `to`/`cc`/`bcc`/
  `subject` frontmatter, with the same contact fuzzy-finder used for new
  drafts. Submitting rewrites *only* those four frontmatter fields in
  place, preserving the body and every other frontmatter field, and does
  not open `$EDITOR`. Previously, changing recipients on an existing
  draft meant hand-editing the YAML frontmatter. `c` is a no-op with a
  status hint outside the Drafts mailbox, and drafts with missing/
  malformed frontmatter fail with an error status and no data loss.

### Fixed
- **Only the row under the cursor is highlighted in the TUI email list.**
  Rows toggle-selected with `v` kept a full-row background even after the
  cursor moved on, so several rows looked equally focused and it was
  ambiguous which email the next keystroke would act on. Selected rows now
  show the checked checkbox in the marker column plus the selection
  foreground color, and the background fill belongs to the cursor row alone.
- **File permissions survive a draft rewrite.** Every write that goes through
  `write_atomic` (approve, demote, mark-as-sent, recipient edits) renames a
  fresh temp file over the target, which replaced the inode and reset a draft
  the user had chmod'ed to 0600 back to the umask default. The target's mode
  is now copied onto the temp file before its content is written; newly
  created files still follow the umask.
- **The TUI cursor no longer jumps to a different email when the list changes
  underneath it.** The selection is now anchored to the selected email's
  identity (its file path) across every list rebuild: approving or demoting a
  draft, new mail arriving via background sync, batch archive/delete/move, and
  switching accounts all keep the cursor on the same email (falling back to
  the nearest surviving row when that email left the list). Previously the
  cursor was a bare positional index that every re-sort silently re-pointed,
  so keystrokes already in flight could archive or delete the wrong email.
- **`mark-approved`, `mark-draft`, and marking a draft as sent no longer
  destroy frontmatter.** These operations previously round-tripped the
  frontmatter through the typed struct, silently deleting the `date:` field
  and any unknown user fields (and adding `cc: null` noise); losing `date:`
  also made an approved draft re-sort to the bottom of the Drafts list. They
  now rewrite only the affected lines (`status:`, and for sent also
  `sent_at`/`sent_via`/`message_id`), preserving everything else byte for
  byte.
- **iMIP RSVP replies now include `DTSTART`/`DTEND`, fixing Exchange/Outlook
  rejection.** A `METHOD:REPLY` built by `mp invite accept|tentative|decline`
  previously omitted the event's start/end times. RFC 5546 marks `DTSTART`
  optional in a REPLY, but Exchange/Outlook reject one that lacks it (the
  reply is delivered as an unusable `not supported calendar message.ics` with
  "Invalid ICAL element: DTSTART"). The REPLY now echoes the source invite's
  `DTSTART` and `DTEND` (or `DURATION`) verbatim — value and parameters such
  as `TZID` or `VALUE=DATE` preserved exactly, matching what Outlook itself
  sends. If the invite genuinely carried no `DTSTART`, the reply is still
  built without one (and a warning logged) rather than failing. Ticket #0029.

### Changed
- **CLI binary renamed `email` -> `mp`.** The installed binary is now
  `mp` (`cargo install --path .`); Homebrew installs it as `mp` too. All
  `--help` text, docs, README, and website command examples use `mp`
  (e.g. `mp fetch`, `mp config show`). The user-facing name/version
  string stays `mailypoppins X.Y.Z`. The Cargo package/library remain
  named `email` internally (invisible to users). The Homebrew tap moved
  to `brew tap sylvainHellin/mailypoppins` / `brew install mailypoppins`.
  Config paths (`~/.config/email/`), the notification app name, and log
  file names are unchanged. Ticket #0022 / #0013.

### Security
- **`accept_invalid_certs` is now restricted to loopback hosts.** The
  per-account cert-validation opt-out exists for Proton Mail Bridge,
  which always listens on `127.0.0.1` with a self-signed cert. It was
  previously honoured for *any* host, so a config pointing at a remote
  server with `accept_invalid_certs = true` would silently hand
  credentials to an active man-in-the-middle. All four TLS paths (IMAP
  connect, SMTP send, config-wizard connection tests, OAuth2 SMTP test)
  now call a shared guard (`ensure_invalid_certs_allowed` in
  `src/config.rs`) that refuses with a clear error when the flag is set
  for a non-loopback host (anything other than `localhost`, `127.0.0.0/8`
  or `::1`). Loopback behavior is unchanged. There is no override: no
  documented setup needs invalid certs on a remote host, and a user who
  genuinely does can tunnel through loopback.

- **Secret files are created with mode 0600 atomically.** The encrypted
  secrets store (`secrets.enc`) and OAuth2 token caches (`tokens/*.enc`)
  were written first and chmod'd 0600 afterwards, leaving a brief window
  where umask-default (often world-readable) permissions applied. A new
  shared helper (`write_secret_file_atomic` in `src/secrets.rs`) opens
  the temp file with `create_new(true)` and `mode(0o600)` so the
  restrictive mode holds from the very first byte, then renames over the
  destination -- overwrite semantics and crash-safe atomic replacement
  are preserved, and a stale temp file from a crashed run is cleaned up
  instead of blocking the write.

- **Saved HTML email bodies now carry a restrictive Content-Security-Policy.**
  Fetched HTML companions (`.html` next to the `.md`) are opened via
  `file://` in the default browser (`b` in the TUI), so a hostile email
  could previously run scripts and load remote tracking pixels. At save
  time we now inject
  `<meta http-equiv="Content-Security-Policy" content="script-src 'none';
  connect-src 'none'; img-src data: cid: file:">` into the HTML
  (`inject_csp_meta` in `src/parse.rs`): scripts and script-initiated
  network access are blocked, and remote (http/https) images -- i.e.
  tracking pixels -- are blocked by default. `file:` stays allowed for
  images because inline `cid:` references are rewritten to local
  `file://` attachment paths at save time; with scripts and connections
  blocked there is no exfiltration channel, so local images are safe.
  Sender-supplied CSP meta tags are stripped and replaced so our policy
  always wins; injection is idempotent on re-save. Applies to newly
  fetched emails (previously saved `.html` files are unchanged). No
  config opt-out for remote images yet: the save path does not currently
  receive the global config, so plumbing it through would touch four
  call sites -- deferred until someone actually wants remote images.

  *Hardened after security review:* the initial implementation searched
  for the first `<head>`/`<html>` in a lowercased copy of the HTML.
  Two blockers: (1) a fake head hidden in a comment
  (`<!--<head>-->`) or attribute value lured the tag to a spot the
  browser never parses as head content, fully neutralizing the CSP;
  (2) byte offsets computed on `to_lowercase()` output misalign in the
  original for characters like `İ` (U+0130, 2 bytes → 3 bytes), so a
  crafted email could panic the sync on a non-char-boundary slice. The
  tag is now *prepended* at the very start of the document (after a
  leading doctype) -- the HTML parser hoists an early `<meta>` into the
  implicitly created `<head>` before any attacker bytes are parsed, so
  no search and no offset math on transformed strings. The same
  Unicode-offset fix was applied to `ensure_utf8_charset` (worst case
  there was mojibake or the same panic). Regression tests cover
  comment-fake-head, attribute-fake-head, and the `İ` expansion case.

### Infrastructure
- **CI + release pipeline (#0011).** New GitHub Actions workflows:
  `ci.yml` runs `cargo test` on every push to `main` and every PR;
  `release.yml` fires on `v*` tag pushes, creates a GitHub release with
  notes extracted from the matching CHANGELOG section, and attaches
  `mailypoppins-<target>.tar.gz` + `.sha256` artifacts for
  `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, and a fully static
  `x86_64-unknown-linux-musl` build (via a new optional
  `vendored-openssl` cargo feature that statically links OpenSSL; not
  compiled in default builds). Release procedure documented in
  [docs/release-process.md](docs/release-process.md).
- **Homebrew tap, repo side (#0013).** Formula template
  (`packaging/homebrew/mailypoppins.rb.tmpl`) installing prebuilt
  release binaries (native macOS arm64/x86_64, static musl on Linux),
  a renderer (`scripts/update-homebrew-formula.sh`) that fills version
  and SHA-256 placeholders from published release checksums, and a
  `homebrew-tap` release job that pushes the rendered formula to
  `sylvainHellin/homebrew-email` on every tag. The job skips itself
  with a notice until the tap repo and `HOMEBREW_TAP_TOKEN` secret
  exist (one-time manual setup, see
  [docs/release-process.md](docs/release-process.md#homebrew-tap)).

### Features
- **Desktop notifications for new mail (#0009).** Opt-in via a new
  top-level `notifications = true` key in config.toml (default: off).
  While the TUI runs, a completed background quick-sync that saved
  genuinely new *inbox* emails fires one desktop notification per sync:
  "sender: subject" when exactly one email arrived, "N new emails" when
  several (natural grouping -- one notification per IDLE-triggered
  fetch, never per email). Read-flag updates, dedup, reconciliation
  moves, and non-inbox mailboxes never notify; skipped duplicates are
  filtered out by matching against the message IDs the save actually
  wrote. Zero new dependencies: shells out to `osascript` on macOS and
  `notify-send` on Linux, degrading silently when the tool is missing.
  Injection-safe by construction: subjects/senders are
  attacker-controlled, so text is passed as separate argv entries (the
  macOS path hands it to AppleScript via `on run argv`, so it never
  touches script source), control characters are stripped, length is
  capped, and a leading `-` is neutralized so text can't be parsed as a
  CLI option (`src/notify.rs`, unit-tested). `email config show` prints
  the setting; `config init` templates include the key.
- **Quick-move emails between mailboxes (`M`, #0018).** Pressing `M` in
  the email list opens a small fuzzy picker of destination mailboxes
  (type-to-filter subsequence match, arrows/Tab to navigate, Enter to
  confirm, Esc to cancel). Works on the current selection if any,
  otherwise the cursor email. The move runs server-side (IMAP
  `UID COPY` + `\Deleted` + `EXPUNGE` -- the same machinery as archive,
  so servers without the MOVE extension work and read/unread flags are
  preserved; Graph accounts use the `/move` endpoint), then the local
  `.md`/`.html`/`_attachments` files follow with the frontmatter
  `status:` updated to match the destination. On IMAP failure the local
  move is rolled back, mirroring archive semantics, and both the source
  and destination mailbox caches are invalidated by index so the
  restored email reappears even if the user switched mailboxes while
  the move was in flight. The picker excludes
  the active mailbox (moving to the same mailbox is impossible by
  construction) and local-only mailboxes like Drafts; the in-memory
  message-ID index is updated for both source and destination.
  Internally `archive_email_locally` / `archive_email_graph` are now
  thin wrappers over the generalized `move_email_locally` /
  `move_email_graph`.
- **Configurable TUI color themes.** New top-level `theme = "..."` key
  in config.toml selects a named built-in theme, helix-style. Built-ins:
  `catppuccin-mocha` (the default -- reproduces the previous hardcoded
  appearance exactly), `catppuccin-latte` (light) and `tokyo-night`
  (plus case-insensitive aliases like `catppuccin`, `latte`,
  `tokyonight`). The old raw Catppuccin palette constants in
  `src/tui/theme.rs` were replaced by a semantic `Theme` struct
  (slots like `unread`, `selection`, `border_focused`, `error`, ...)
  so themes only map meanings to colors and the renderers in
  `src/tui/ui/` never hardcode a palette. The theme is resolved once at
  TUI startup into a process-wide `OnceLock`; an unknown name falls
  back to the default and logs a warning to the activity log instead of
  failing. `email config show` prints the active theme; `config init`
  templates include the key. Website config page documents the option.
  Closes [#0023](docs/tickets/0023-enable-theme-config.md).

- **Terminal-adaptive `terminal` theme.** A new built-in theme (aliases
  `transparent` / `ansi`) that follows the terminal's own palette: the
  background/foreground slots use `Color::Reset` so the terminal's
  default colors show through, and the accent/status slots use the 16
  ANSI named colors (`error` → red, `warning`/`code` → yellow,
  `success` → green, `accent` → blue, ...) so the whole TUI tracks a
  light or dark terminal theme. One deliberate exception: `surface` is
  `DarkGray` (not `Reset`) because the cursor row, status bar and code
  blocks paint over `surface` and rely on it contrasting with `bg`;
  `selection` is `White` so the cursor-row text stays legible on that
  surface. Registered in `THEME_NAMES`, the unknown-name warning list,
  the `config init` template comment and the website config page.

- **Open the log file from the TUI.** New global hotkey `Ctrl+l`
  suspends the TUI and opens the newest daily log file
  (`<data_dir>/logs/mailypoppins-YYYY-MM-DD.log`) in `$EDITOR`, using
  the same suspend/restore mechanism as draft editing. Complements the
  in-app activity log overlay (`L`) which only shows recent status
  messages -- the log file carries full debug-level detail. If no log
  file exists yet, a warning status message is shown instead. Newest
  file is picked by filename (`latest_log_file` in `src/config.rs`;
  daily-dated names make lexicographic order equal date order). Help
  overlay and website `commands.astro` updated alongside.
  Closes [#0025](docs/tickets/0025-access-detailed-logs.md).

- **Open config.toml from the TUI.** New global hotkey `Ctrl+e` suspends
  the TUI and opens the global config file (`~/.config/email/config.toml`)
  in `$EDITOR`, reusing the same suspend/edit/resume mechanism as draft
  editing and the `Ctrl+l` log-open. Config is read once at startup (the
  theme is an `OnceLock` by design), so there is no hot-reload: on return
  the status line notes that changes apply on restart. A missing config
  file yields a warning pointing at `email config init` rather than
  launching the editor on a nonexistent path. Help overlay and website
  `commands.astro` updated alongside.

- **Mark approved drafts back as draft.** New TUI hotkey `D` in the
  email list demotes an approved draft back to `draft` status -- the
  exact reverse of `A`. Useful when `A` was pressed by mistake or
  the draft needs another round of edits before sending. Single-email
  path is direct (no confirm, since it is non-destructive and fully
  reversible by `A`); multi-select uses the same confirm dialog
  pattern as `BatchApprove`. Status guard rejects `sent` / `inbox` /
  `archived` so we never silently rewrite a synced server email.
  CLI parity via `email mark-draft <file>`. Help overlay and website
  pages (`commands.astro`, `faq.astro`) updated alongside.
  Closes [#0021](docs/tickets/0021-mark-as-draft.md).

- **Auto-fetch on TUI startup.** Each account now runs an automatic
  per-account quick sync at launch, so mail that arrived between TUI
  sessions appears without pressing `s`. New `Action::FetchAccount(idx)`
  variant performs a quick sync against a specific account using that
  account's own `imap_config` / `graph_config` / `message_id_index` /
  `mailbox_states`; the trigger lives in the `BgResult::IndexReady`
  handler in `src/tui/bg.rs`, so each account fires as soon as its own
  index lands -- a slow reconcile on one account does not block a fast
  one. Local-only accounts (no IMAP, no Graph) are skipped. Combined
  with #0002, the cold-start fetch is now the warm-state ~1-2 s path
  instead of the previous 14 s reconcile, so the user sees fresh mail
  almost immediately. Closes [#0001](docs/tickets/0001-auto-fetch-on-tui-startup.md).

### Performance
- **Search narrows incrementally per keystroke.** Appending a
  character in `/` or `\` search now retain-filters the current
  visible set instead of rescanning the full mailbox: substring
  matching is monotone under query extension, so the new match set is
  a subset of the previous one and each keystroke only tests the
  emails that still matched the previous prefix (a big win for content
  search, which lowercases whole bodies per comparison). The needle is
  lowercased once per keystroke. Backspace and query resets recompute
  from the full list, as do appends where lowercasing rewrites earlier
  characters (Greek capital sigma is context-sensitive: "ΘΕΟΣ" lowers
  to "θεος" but "ΘΕΟΣΦ" to "θεοσφ", so the extended query can match
  entries the shorter one missed) — the narrow path is taken only when
  the old lowercased query is a prefix of the new one. Unit tests
  assert narrow-equals-full equivalence, stale-index safety, and the
  final-sigma fallback.
- **Mailbox switches, account switches and search no longer deep-clone
  the email list.** Cache slots and the active list are now
  `Arc<Vec<EmailEntry>>` (P2): switching mailboxes/accounts and
  delivering a background `MailboxLoaded` result share the allocation
  (`Arc::clone`) instead of cloning every entry (each `EmailEntry`
  carries the full parsed body -- on a multi-thousand-email mailbox
  every switch previously copied megabytes). The search filter is now a
  `visible: Vec<usize>` index view over the unfiltered list instead of
  a cloned filtered `Vec`; rendering, navigation, selection and all
  actions resolve the selected email through this indirection
  (`App::selected_email` / `visible_emails`), so actions under an
  active filter still target the right underlying entry. Mutations
  (optimistic archive/delete removal, read-flag updates) go through
  `App::with_emails_mut`, which drops the cache slot's strong reference
  first so `Arc::make_mut` mutates in place in the common case and
  re-shares the updated Arc with the slot afterwards -- a deep clone
  only happens when another owner (the per-account cache mirror from
  `save_to_account`) still shares the allocation. Behavior preserved:
  cursor clamping on switch/removal, ordering, read-status display,
  empty-search shows all, optimistic list updates, and the
  `MailboxLoaded` generation guard from the previous entry. One
  deliberate fix on top: switching back to an account with a saved
  search query now reapplies the filter instead of silently showing
  the unfiltered cache. Unit tests cover the visible-index mapping,
  removal-under-filter, and Arc sharing (`src/tui/app/keys.rs`).
- **Mailbox loads no longer block the UI thread.** `load_emails`
  (walkdir + frontmatter parse of every `.md` in a mailbox, seconds on
  large mailboxes) previously ran synchronously on every cache miss:
  post-fetch reloads, editor returns, and sidebar/hotkey mailbox
  switches would all freeze the TUI. The walk now runs on a background
  thread (following the `BgResult::IndexReady` pattern) via a new
  `Action::LoadMailbox` / `BgResult::MailboxLoaded` pair; the spinner
  shows while it runs. Same-mailbox reloads keep the stale list visible
  until the fresh entries arrive (no flicker, no empty state), then swap
  it in with the cursor clamped as before. Cache-miss switches to a
  *different* mailbox (or account) show an empty list with a
  "Loading <mailbox>..." status instead of the previous mailbox's
  content. Stale results are dropped via account/mailbox index checks
  plus a generation counter (`App::mailbox_load_generation`) that is
  bumped on every request and on optimistic archive/delete list
  mutations, so an out-of-order or pre-mutation walk can never clobber a
  newer list or resurrect a removed email (guard logic + tests in
  `src/tui/bg.rs::mailbox_loaded_is_current`).
- **No-op fetches no longer invalidate caches or reload the open mailbox.**
  `SyncResult` (IMAP and Graph) now carries `touched_dirs`: the local
  mailbox directories the sync actually modified on disk (new emails
  saved, read flags updated, duplicates removed, reconciliation
  moves/removals -- a move lists both source and destination). The TUI
  quick-sync paths thread this through `BgResult::Fetch`, and the
  handler in `src/tui/bg.rs` now: skips cache invalidation and the
  UI-thread mailbox reload entirely when nothing changed (the common
  case for IDLE-triggered fetches), and invalidates only the touched
  mailboxes' caches otherwise -- reloading the open mailbox only when it
  was among them. Full sync (`BgResult::Sync`) keeps the conservative
  invalidate-everything behavior, as does the error path
  (`touched_dirs: None`). Selection/scroll preservation in
  `reload_current_mailbox` is unchanged.
- **Saving fetched emails no longer triggers a full-directory re-scan per
  new email.** `save_fetched_emails_with_known_ids` now returns the
  `(message_id, path)` of every file it writes, and `sync_mailboxes`
  updates its in-memory index directly from those paths instead of
  re-walking the whole mailbox directory and substring-matching each
  Message-ID against full file contents (previously O(new_emails ×
  files), and a Message-ID quoted in another email's body could
  false-match). The TUI search-result save path uses the returned path
  too; its duplicate-hit fallback now looks up the Message-ID in
  frontmatter only (via `scan_mailbox_message_ids`) rather than
  substring-matching whole files. Both `find_file_by_message_id*`
  helpers are deleted. The Graph sync path never had this re-scan.
- **TUI cold start is now ~instant instead of ~1.4 s black screen.**
  `AccountState::new` no longer walks every mailbox directory to build
  the in-memory `message_id_index` synchronously. The scan is extracted
  into a free function `build_message_id_index` and dispatched on a
  background thread per account from `run_loop`; results arrive via the
  new `BgResult::IndexReady` variant, which sets `acct.message_id_index`
  and clears `acct.indexing`. The status bar shows "Indexing..." with
  the existing spinner; sync/fetch actions queue via the existing
  `bg_count > 0` gate and fire automatically when the last account
  finishes. Local benchmark: per-account `AccountState::new` dropped
  from 1183 ms / 234 ms (TUM / Proton) to 13 ms / 4 ms.
  Closes [#0003](docs/tickets/0003-cold-start-async-indexing.md).
- **First quick sync after launch is now <2 s instead of ~14 s.** Persist
  per-role IMAP `MailboxState` (`uid_validity`, `uid_next`, `exists`) to
  `<account_dir>/mailbox-states.json` after every successful Fetch /
  Sync, and reload it in `AccountState::new`. This lets the cold-start
  reconcile decision in `sync_mailboxes` take the state-based skip
  branch instead of falling through to a full Message-ID scan of INBOX +
  Archive. `uid_validity` mismatch on the next SELECT still falls back
  to a full reconcile, so other-client moves / mailbox renumbering
  remain safe. Closes [#0002](docs/tickets/0002-persist-mailbox-states.md).

### Fixed
- **Read/unread status now survives fetches and syncs reliably in both
  directions.** Three bugs conspired to make `\Seen`/`isRead` sync flaky
  (ticket [#0004](docs/tickets/0004-fix-read-unread-sync.md)):
  1. *Fetches clobbered in-flight local marks.* Sync captured the server
     read flags first and applied them to local frontmatter seconds
     later, so marking an email read while a sync was running (previewing
     mail during the startup auto-fetch, for instance) was silently
     reverted to the older server snapshot. The flag-apply helpers now
     take a snapshot cutoff captured before the server read and skip any
     file modified at-or-after it (with 1 s slack for coarse filesystem
     mtimes): the newer local state wins, and its own server propagation
     -- already in flight -- converges everything on the next sync.
  2. *Webmail read changes rarely propagated to local files (IMAP).* The
     quick-sync "adaptive probe" returned early when the newest 10 UIDs
     were all known -- the common no-new-mail case -- so read/unread
     changes made in another client on anything but the 10 newest
     messages never reached the local `read:` field. Pass 1
     (headers + FLAGS, ~50 bytes/msg) now always covers the full
     quick-sync window; pass 2 (body download) is still skipped when
     nothing is new, so the latency cost is one small FETCH.
  3. *Graph accounts could never mark some emails read.* The Graph read
     sync substring-matched `read: true` against the whole file, so an
     email whose body merely quoted that string was permanently stuck
     unread locally. Both backends now share the same frontmatter-aware,
     cutoff-guarded helper (`sync_local_read_flags`).
  Regression tests cover both backends through the shared helper in
  `tests/sync_integration.rs` and `src/imap_client/sync.rs`.
- **Graph attachment filenames are now sanitized before writing to disk.**
  The Graph fetch path used the server-provided attachment name verbatim, so a malicious sender could name an attachment `../../evil` and have it written outside the `_attachments/` directory.
  The name now goes through the same `sanitize_attachment_filename` helper the IMAP path already used, which replaces path separators and control characters and caps the length.
- **New-draft skeletons no longer hard-code `attachments: []`.**
  The CLI `email new`, TUI `n`, and compose wizard skeletons wrote flow-style `attachments: []`, which deserializes to `Some(vec![])` instead of `None` and diverges from every other empty frontmatter key.
  They now emit the bare `attachments:` key, matching `to:` / `cc:` / `reply_to:`, and the CLI/TUI skeletons are deduplicated into a shared `new_draft_skeleton` helper in `src/draft.rs`.
- **Forward drafts no longer break when the source email is archived.**
  `create_forward_draft` previously canonicalised the per-mailbox
  `<inbox>/<stem>_attachments/<file>` paths into the draft frontmatter.
  As soon as the source moved (manually, or via reconcile after an
  archive on the server), the draft pointed at a path that no longer
  existed and `email send` failed with `Failed to read attachment`.
  Each fetched attachment is now also hardlinked into a per-account
  stable mirror at `<account>/attachments/<sanitized-message-id>/`
  (with a copy fallback on filesystems that disallow hardlinks).
  Forward drafts reference the stable path, which survives any
  subsequent inbox -> archive move. Pre-existing emails lazy-hydrate
  the stable dir on first forward, so the fix applies to historical
  mail without a one-shot migration. The reconcile "no longer on
  server" branch in `src/sync.rs` cleans up the stable dir alongside
  the per-mailbox copy. New helpers in `src/parse.rs`:
  `sanitize_message_id_for_path`, `stable_attachments_dir`,
  `link_or_copy`, `account_dir_for_email`. Closes
  [#0006](docs/tickets/0006-attachment-paths-after-archive.md).

### Changed
- **All app data moved to a single OS-conventional data directory.**
  Mail tree, drafts, contacts cache, OAuth2 tokens, and logs now live
  under `mailypoppins_data_dir()`:
  - macOS: `~/Library/Application Support/mailypoppins/`
  - Linux (incl. WSL): `$XDG_DATA_HOME/mailypoppins/` (default `~/.local/share/mailypoppins/`)

  Layout: `accounts/<name>/{inbox,archive,sent,drafts,<extra>}/`,
  `accounts/<name>/contacts-cache.json`, `tokens/<name>.enc`, `logs/`.
  `~/.mailypoppins/` is retired entirely. Override the root with the
  `MAILYPOPPINS_DATA_DIR` env var (test seam + portable-install escape
  hatch).
- New `src/config.rs` helpers: `mailypoppins_data_dir`, `account_dir`,
  `mailbox_dir`, `drafts_dir`, `contacts_cache_path`, `tokens_dir`,
  `logs_dir`. Replace the removed `resolve_root_dir`,
  `resolve_mailbox_local_path`, `resolve_drafts_dir_from_config`,
  `resolve_dir`, `expand_path`, `log_base_dir`.
- Wizard prints the derived data-dir paths instead of asking the user to
  pick one. The Obsidian-vault workflow is preserved via a symlink hint:
  `ln -s ~/Library/Application\ Support/mailypoppins/accounts/<name> ~/notes/email/<name>`.

### Removed
- Per-account `[accounts.directories]` block (`root`, `drafts` keys).
- Per-mailbox `local = "..."` field on `[accounts.mailboxes.{inbox,archive,sent}]`
  and `[[accounts.mailboxes.extra]]`. `MailboxMapping` now carries only `server`.
- `email config migrate` subcommand (legacy single-account -> multi-account
  migrator). Per the v1.0 "no migrations" policy, configs that still use the
  old keys are rejected at parse time with a clear message instructing the
  user to re-run `email config init`.

### Breaking
- Existing configs containing `[accounts.directories]` or per-mailbox
  `local = "..."` fail to load. Run `email config init` to regenerate.
- Existing local mail trees at user-chosen roots (e.g. `~/notes/email/`)
  are no longer read or written. Re-run `email config init`, then
  `email sync` to repopulate the new data dir from the server. (Or move
  the existing tree manually into
  `<data_dir>/accounts/<name>/{inbox,archive,sent,...}/`.)
- OAuth2 token caches at `~/.mailypoppins/tokens/<account>.enc` are
  ignored. Re-run `email config oauth2-login --account <name>`.

### Added
- `dirs = "5"` dependency for cross-platform data-dir resolution.

### Fixed
- **Inbox sort key now respects hours / minutes across timezones.**
  `resolve_date` previously formatted the RFC2822 / RFC3339 timestamp
  in the sender's local offset, so two emails on the same calendar day
  sent from different timezones sorted by sender-local wallclock instead
  of by actual UTC instant -- making same-day ordering look random.
  The sort key is now normalised to UTC (`with_timezone(&chrono::Utc)`)
  before formatting; display strings stay in sender-local time so dates
  still match other clients. Regression test
  `test_resolve_date_sort_normalises_timezone` covers both branches.
  Closes [#0024](docs/tickets/0024-sorting-email-inbox.md).
- `email config init` and `email config add-account` no longer panic with
  "Cannot start a runtime from within a runtime" when the wizard reaches
  the IMAP test, OAuth2 device-code flow, or Graph folder discovery.
  `main` is `#[tokio::main]`, so the wizard's sync code paths now drive
  nested async work via a new `config_cmd::helpers::run_async_blocking`
  helper that detects the existing runtime and spawns a fresh one in a
  dedicated OS thread (mirrors `oauth2::load_or_refresh_token_blocking`).

### Changed (previous unreleased batch)
- **Default password store switched from OS keyring to a machine-bound
  encrypted file** at `~/.config/email/secrets.enc`. Built on
  ChaCha20-Poly1305 + HKDF-SHA256 with the key derived from
  `machine-uid + getuid + app salt`. Pure Rust, zero prompts at runtime,
  identical UX on macOS, Linux, and WSL. The file is undecryptable on any
  machine other than the one that wrote it (defends against Time Machine /
  iCloud / Dropbox / accidental git commit leakage).
- OAuth2 token caches now stored encrypted at
  `~/.mailypoppins/tokens/<account>.enc` using the same crypto.
- New `email config reset-secrets` command -- wipes the secrets file and
  token caches, then walks each account prompting for re-entry. Use after
  a Time Machine restore to a new machine.
- `keyring` retained as an **opt-in backend** via
  `secrets_backend = "keyring"` in `~/.config/email/config.toml`.
- New `src/secrets.rs` module exposing `SecretsBackend` trait,
  `EncryptedFileBackend`, `KeyringBackend`, and `encrypt_blob` /
  `decrypt_blob` helpers reused by `oauth2.rs`.
- See [docs/secrets.md](docs/secrets.md) for the threat model, key
  derivation, file layout, and recovery procedure.

### Removed
- `scripts/codesign-macos.sh` and the `scripts/install.sh` wrapper. No
  longer needed -- secrets do not depend on a stable code-signing
  identity, so `cargo install --path .` is the canonical install command
  again.
- `keyring`-to-encrypted-file migration command. Per project policy ("no
  migration paths until v1.0"), users on the old keyring path re-enter
  passwords once via `email config init` or `email config set-password`.
- `windows-native` feature on the `keyring` crate. Native Windows is not
  a supported target (WSL only).

### Breaking
- After upgrading, run `email config init` (fresh setup) or
  `email config set-password <smtp|imap> --account <name>` for each
  configured account to populate `~/.config/email/secrets.enc`. Existing
  Keychain entries are ignored unless you opt into
  `secrets_backend = "keyring"` in `config.toml`.
- OAuth2 token cache file extension changed from `.json` to `.enc`. Run
  `email config oauth2-login --account <name>` to re-acquire and cache
  tokens for each OAuth2 / Graph account.

### Fixed
- **Compose-wizard and search inputs now scroll horizontally to keep the
  cursor visible (TKT-0046).** In the compose/"Edit recipients" overlay, the
  To/Cc/Bcc/Subject lines used to render from the left with no scrolling, so
  once a field grew past the field width (typically after one or two
  recipients with display names) the caret and newly typed text walked off the
  right edge — input still worked but was invisible. The active field now
  scrolls so the end of the text (where the append-only caret sits) is always
  on screen, with a leading `…` when content is hidden to the left. The same
  width-aware `visible_window` helper fixes the email-list search line, the
  server-search input, the Contacts fuzzy-search input, and the directory /
  mailbox picker inputs. Slicing is Unicode display-width-aware (umlauts and
  CJK no longer misalign or risk a panic).

## [0.8.0] - 2026-04-08

### Added
- **Contact autocomplete**: mine `from:`/`to:`/`cc:` headers from each
  account's local mail archive, filter noreply/bulk-domain noise, rank
  contacts with a tiered comparator (`sent_to` > `sent_cc` > `received`)
  and a 180-day-half-life frecency tiebreaker. Cached per account at
  `<root>/.contacts-cache.json`.
- New `email contacts` CLI subcommand tree:
  - `rebuild` (re)builds the index from local mail for one or all
    accounts.
  - `stats` shows totals and the top 10 ranked contacts for an account.
  - `search <query>` returns ranked fuzzy matches. `--parsable` emits
    tab-delimited `email\tname` lines compatible with mutt
    `query_command`, aerc `address-book-cmd`, himalaya-vim, and
    vim `completefunc` completion sources.
- **TUI compose wizard overlay** triggered by `n` (new) and `w`
  (forward), with a four-field form (`To`/`Cc`/`Bcc`/`Subject`), live
  fuzzy-matched suggestions under the focused address field, Tab
  cycling, Ctrl+g force-submit, Ctrl+u clear-field, and Esc cancel.
  Reply keys `r`/`R` stay direct-to-editor as before. Aerc-style split
  on the last comma so multi-recipient fields work naturally; accepted
  suggestions render as `"Display Name" <addr>, ` so the user can keep
  typing. Once submitted, the draft file is written with a populated
  frontmatter block and then handed off to `$EDITOR`.
- Incremental contacts-index hooks: every successful send (CLI or TUI)
  bumps the recipients' `sent_to`/`sent_cc` counters, and every sync
  merges freshly-fetched email headers into the active account's index,
  preserving historical `first_seen` dates. Both hooks are best-effort
  and no-op when no `.contacts-cache.json` exists yet.
- `scripts/install.sh` and `scripts/codesign-macos.sh` for stable macOS
  code signing during local development. After one-time setup of a
  self-signed cert (see `CONTRIBUTING.md`), the Keychain no longer
  re-prompts on every `cargo install --path .` rebuild.
- `CONTRIBUTING.md` with build instructions, the macOS keychain setup
  walkthrough, and troubleshooting notes.
- `src/contacts/` module (`types`, `filter`, `rank`, `extractor`,
  `matcher`, `cache`, `hooks`) built on `nucleo-matcher` for fuzzy
  matching and `mailparse::addrparse` for robust multi-recipient
  header parsing.
- 47 new unit tests across filter, rank, extractor, matcher, hooks,
  and the wizard recipient-field normalizer (total now 236).

### Changed
- `AGENTS.md` and project `CLAUDE.md` both now point at
  `./scripts/install.sh` as the canonical install command.
- TUI `n` key now opens the compose wizard instead of writing a
  skeleton directly. `Action::NewDraft` is kept as a dead-code
  fallback path.
- TUI `w` key now opens the wizard with the `Fwd:` subject
  pre-populated and attachments preserved through
  `create_forward_draft` before the frontmatter is patched with the
  wizard's edits.
- `cargo clippy -- -D warnings` is clean again: collapsed a nested
  `if` in `src/imap_client/sync.rs` that was already failing the lint.

### Fixed
- Creating a new email via the TUI `n` key now writes a fully populated
  frontmatter block (`to`/`cc`/`bcc`/`subject`/`from`/`date`) instead
  of an empty skeleton. Closes the "data is not added to the
  frontmatter" bug noted in `roadmap.md`.
- Wizard now strips trailing commas and whitespace from the
  `to`/`cc`/`bcc` fields before writing the draft, so contacts with
  display names that contain a comma (for example
  `"Doe, Jane" <addr>`) no longer break
  `mailparse::addrparse` and fail the subsequent send.

### Dependencies
- New: `nucleo-matcher = "0.3"`, `regex = "1"`, `serde_json = "1"` (the
  last one was already a transitive dep).
- Avoided `once_cell` as a direct dep by using `std::sync::LazyLock`
  (Rust 1.80+).

## [0.7.4] - 2026-04-05

### Added
- Mark as read / unread: persistent local tracking via `read` field in frontmatter, synced with IMAP `\Seen` flag
- Auto-mark-as-read when previewing emails (cursor moves to a new email in Inbox/Archive/Extra mailboxes)
- Manual toggle with `m` key (single email or batch selection)
- Unread indicator in email list: blue `\u{f444}` dot for unread, bold text for unread, dimmed text for read
- Unread count shown in status bar (e.g. "3 unread")
- IMAP fetch now captures FLAGS alongside body (`BODY.PEEK[] FLAGS`), preserving server-side read status
- Bidirectional read status sync: server `\Seen` flag synced to local `read:` frontmatter on each fetch
- `mark_read_on_server()` / `mark_unread_on_server()` IMAP operations
- `update_read_status_locally()` for frontmatter file updates
- `sync_local_read_flags()` updates existing local emails' read status from server during fetch/sync
- 16 new tests for read/unread functionality (types, ops, sync, frontmatter)

### Changed
- Existing emails without `read:` field default to unread (backward compatible)
- Optimistic updates: local state updates immediately, server follows async with rollback on failure
- Pass 1 of two-pass fetch now also retrieves FLAGS, enabling read status sync for existing emails

## [0.7.3] - 2026-04-04

### Changed
- Deduplicated message-ID scanning: canonical `scan_mailbox_message_ids` in `parse.rs`, both `scan_existing_message_ids` and `scan_local_message_ids` delegate to it
- Replaced manual `format!()` YAML construction in `save_fetched_emails` with `SaveFrontmatter` struct + `serde_yaml` for correct special-character quoting
- Extracted shared `collapse_hyphens` helper to `types.rs`, used by all three slugify functions
- Email validation in `validate_draft` now uses `lettre::message::Mailbox` (RFC 5321) instead of naive `contains('@')` check
- Added `#[derive(Default)]` to `FetchCriteria` for cleaner construction

### Added
- 28 new tests for `imap_client` submodules (search.rs: 22, fetch.rs: 6), restoring coverage lost during module split
- `SaveFrontmatter` struct in `types.rs` for type-safe frontmatter serialization
- `collapse_hyphens` helper in `types.rs`

### Fixed
- Sync now uses cross-directory dedup: emails already stored in any local mailbox
  directory (e.g. Archive) are skipped when fetching from other mailboxes (e.g. INBOX).
  This prevents re-downloading archived emails if they still exist on the server's INBOX.
- Special characters in email subjects/senders no longer break frontmatter YAML
- Added diagnostic logging to server search: logs which IMAP mailbox each query targets,
  how many results each mailbox returns, and where search results are saved on disk.

## [0.7.2] - 2026-04-04

### Changed
- Refactored large TUI and IMAP modules into focused submodules:
  - `src/tui/app.rs` (1863 lines) -> `src/tui/app/` with `mod.rs` (506), `types.rs` (616), `keys.rs` (824)
  - `src/tui/ui.rs` (1634 lines) -> `src/tui/ui/` with 8 submodules (sidebar, list, headers, preview, status, overlays, search, util)
  - `src/tui/mod.rs` (1304 lines) -> split into `mod.rs` (118), `actions.rs` (678), `bg.rs` (165), `helpers.rs` (367)
  - `src/imap_client.rs` (1530 lines) -> `src/imap_client/` with 6 submodules (fetch, sync, search, watch, ops, batch)
- No file exceeds 824 lines. Zero behavior change.

## [0.7.1] - 2026-04-04

### Added
- Testing infrastructure: 139 tests (110 unit + 29 integration), all offline, <0.5s
- Unit tests for: `parse.rs`, `types.rs`, `send.rs`, `draft.rs`, `config.rs`, `imap_client.rs`, `sync.rs`, `tui/app/types.rs`
- Integration tests: `tests/draft_integration.rs`, `tests/sync_integration.rs`, `tests/save_emails_integration.rs`
- Snapshot tests for `markdown_to_html` via `insta`
- Dev-dependencies: `tempfile`, `insta`

## [0.7.0] - 2026-04-04

### Added
- Proton Mail Bridge support via IMAP STARTTLS and self-signed certificate acceptance
- `accept_invalid_certs` config option for SMTP and IMAP (for Proton Bridge and other self-signed setups)
- IMAP STARTTLS connection mode for non-993 ports (automatic, port-based heuristic)
- Proton Bridge preset in `email config init` wizard (pre-fills localhost:1143/1025 with cert bypass)
- Multi-account support: N email accounts with independent IMAP/SMTP/directories/signatures
- New config format using `[[accounts]]` array in `config.toml`
- Per-account keyring namespacing (`smtp-password-{name}`, `imap-password-{name}`)
- Account switching in TUI: backtick cycles, Ctrl+1-9 for direct jump
- Account selector in status bar with unseen-mail indicators
- Per-account IMAP watchers (all accounts watched simultaneously)
- `--account` / `-A` CLI flag to target a specific account
- `email config migrate` command to convert old single-account config
- `email config add-account` command to add accounts to existing config
- Per-account sidebar titles

### Changed
- Config model: `[smtp]`/`[imap]`/`[directories]`/`[mailboxes]`/`[signatures]` are now nested under `[[accounts]]`
- `default_from` moved from `[smtp]` to `[[accounts]]`
- Status bar simplified: account labels + `? help` replace verbose hotkey hints
- `config set-password` now takes `--account` flag

## [0.6.0] - 2026-03-30

### Added
- Server-side search across all mailboxes
- Background task status panel
- Open HTML version of email in browser
- Open attachments from the TUI (`o`)
- Help overlay with scrolling (`j`/`k`, `d`/`u`, `gg`/`G`) and filtering (`/`)
- Forward emails with attachments
- Batch operations via email selection (delete/archive)
- Paperclip icon for emails with attachments

### Changed
- Performance: shared IMAP connection, skip re-downloading existing emails
- Switched to async-imap
- Streamlined fetch/sync (removed reconcile step)
- Optimistic archive/delete (local state updates immediately)

### Fixed
- Fetching emails no longer marks them as read on the server
- HTML parser charset handling
- Date display in search mode
- UTF-8 character boundary parsing bug

## [0.5.0] - 2026-03-08

### Added
- Full TUI interface (major refactor from CLI-only)

## [0.4.0] - 2026-03-01

### Added
- Case-insensitive mailbox matching (consistent with IMAP)
- Fetching from different mailboxes saves to correct directory

## [0.3.1] - 2026-02-26

### Changed
- Complete refactor into modular components

## [0.3.0] - 2026-02-22

### Added
- Sync sent folder
- Save original `.html` alongside `.md` for received emails, reinject in replies
- Archive and delete commands
- `--yes` flag for non-interactive send
- Watch command (IMAP IDLE)

### Fixed
- UTF-8 string truncation bug

## [0.2.0] - 2026-01-31

### Added
- Copy path of selected file
- Color highlighting in browse view
- Logging and individual send status tracking

## [0.1.0] - 2026-01-16

### Added
- Initial release
- Markdown drafts with YAML frontmatter
- SMTP sending with HTML conversion
- IMAP email fetching
- Mailbox listing
- Signature support
