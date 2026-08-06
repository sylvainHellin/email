# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Fixed
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

### Changed
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
