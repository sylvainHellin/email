# Lessons learned

Non-obvious gotchas, regressions, and hard-won fixes that are easy to forget. Append a new entry whenever you spend more than ~30 minutes discovering something that was not obvious from the code.

Format: short imperative title, one-paragraph description, and (when useful) a code reference.

## `cargo install --path .` leaves a stale `email` binary after the `mp` rename

The CLI binary was renamed `email` -> `mp` (ticket #0022). `cargo install --path .` installs `~/.cargo/bin/mp` but does **not** remove the previously installed `~/.cargo/bin/email`, so a stale old binary lingers and shadows nothing but confuses `which email`. On this dev machine the fix is a symlink so old muscle memory / scripts keep working: `rm -f ~/.cargo/bin/email && ln -s mp ~/.cargo/bin/email` (relative target, resolves within `~/.cargo/bin`). This is a local convenience only: it is not created by any install step and does not ship anywhere. The Cargo package/library were also renamed to `mailypoppins` later, in the same ticket, so imports read `use mailypoppins::...`.

## Renaming the Cargo package needs one `cargo install --path . --force`

`cargo install` tracks what it installed by *package* name, not binary name, so after `email` -> `mailypoppins` (#0022) the documented `cargo install --path .` fails with "binary `mp` already exists in destination as part of `email v0.8.0`". One `--force` fixes it permanently: the `.crates.toml` entry is rewritten and plain `cargo install --path .` works from then on. Worth knowing before assuming the build is broken.

## Renaming the Cargo package renames every `insta` snapshot file

`insta` keys snapshot filenames on the crate name, so `email` -> `mailypoppins` (#0022) turned all 11 committed `.snap` files into misses and wrote 11 `.snap.new` beside them, in `src/snapshots/` and `src/tui/ui/snapshots/`. Do not answer that with `cargo insta accept`: it re-derives the goldens from the code being changed, which is exactly the review a golden frame exists to force. `git mv` each `email__*.snap` to `mailypoppins__*.snap`, then diff the pending `.snap.new` against it ignoring the `assertion_line:` header and confirm every pair is identical before deleting the `.new` files. If a pair is not identical, the rename was not the only thing that changed.

## The config directory move is a location change, not a data migration

The "no migration paths until v1.0" invariant blocked the `~/.config/email` -> `~/.config/mailypoppins` rename for a long time (#0022). It should not have: the invariant is scoped to data formats, secret storage and wire protocols, and a directory rename reads not one byte inside the directory. `config::migrate_legacy_config_dir()` is one `fs::rename` at startup, which is what makes it idempotent and concurrency-safe for free (the loser of a race gets `ENOENT`, and old-absent plus new-present is success). The part worth copying elsewhere is what it refuses to do: there is no read fallback to the old path. A client that quietly keeps reading the old location when the move fails never finishes the move, and every later release has to keep carrying both paths.

## HTML charset must be injected before saving

Incoming HTML bodies are saved as raw bytes from the server. Browsers default to latin-1 when no charset is declared, breaking umlauts and other non-ASCII characters. Always inject `<meta charset="UTF-8">` before writing to disk -- see `ensure_utf8_charset()` in `parse.rs`.

## The `{{SIGNATURE}}` placeholder is the quote boundary, not the signature (post-#0099)

Reply and forward drafts still contain a `{{SIGNATURE}}` placeholder between the reply area and the quoted conversation, and `markdown_to_html` still splits on it at send time so the companion rich-HTML quote is spliced below the reply. Do not remove it: without it the quote falls to end-of-body and the rich companion HTML is dropped. What changed in #0099 is that the placeholder no longer carries the signature. The signature is a per-account Markdown snippet (`config::resolve_signature_markdown`) appended to the draft body *at creation*, above the placeholder for reply/forward and after the body for a new draft, so it is visible and editable. To avoid a double signature, `SendContext.signature` is `None` for draft sends; direct sends and invites keep the send-time append because they have no editable draft. Do not re-populate `SendContext.signature` from config for a draft send.

## Send account is resolved by `from:` address, not active TUI account

At send time, the draft's `from:` field is matched against each account's `default_from` to select the correct SMTP/IMAP/Graph config. Implemented in `resolve_send_account()` in `tui/helpers.rs`. Draft-creation commands (reply, forward, new) auto-insert the active account's `default_from`, but a user could in principle send a draft authored from a different account, and the right config must be used.

## Sent folder dedup uses Message-ID, never UID

Sent `.md` files store `message_id` in frontmatter. Sync skips uploading emails already present on the server by Message-ID. The Sent directory is also never reconciled -- locally-authored files are the source of truth. UIDs are unstable across SELECTs and unusable for dedup across sessions.

## Reconciliation is INBOX + Archive only

Only INBOX and Archive participate in server-driven reconciliation (move/delete detection). Sent is excluded by design (see above). Drafts and other mailboxes are local-only or fetch-only.

## Quoted display names with commas break naive splitting

`"Doe, Jane" <addr>` historically broke send/validate because `.split(',')` split inside the quoted name. Use `split_addresses()` from `parse.rs` for any address-list parsing. Don't reach for `.split(',')` on email headers.

## STARTTLS requires a fake greeting

`async_imap` expects an IMAP greeting line on connect. Plain TCP STARTTLS connections (e.g. Proton Bridge on port 1143) don't have one until after the upgrade. The `ImapStream` wrapper in `imap_client/mod.rs` injects a fake greeting so `async_imap` can negotiate. Implicit-TLS connections (port 993) bypass this.

## Self-signed certificates need `accept_invalid_certs` per account

Proton Bridge ships with a self-signed cert. Both IMAP and SMTP code paths honour an `accept_invalid_certs` flag per account. Don't disable cert validation globally -- keep it per-account. Since the S2 hardening, the flag is additionally restricted to loopback hosts (`localhost` / `127.0.0.0/8` / `::1`) via `ensure_invalid_certs_allowed` in `src/config.rs`; setting it for a remote host is a hard error at connect time.

## Inline images count as attachments

A MIME part with `Content-Disposition: inline` and a filename (PDF, image, etc.) is still an attachment for our purposes. `is_attachment_part()` in `parse.rs` treats inline non-text parts with a filename as attachments so the paperclip icon and `o`/`O` actions work. Earlier versions only matched explicit `attachment` disposition or inline `image/*`.

## BCC-only emails are valid

Frontmatter validation requires at least one of `to`, `cc`, `bcc` to be non-empty. `to` is `Option<String>` with `#[serde(default)]`. Don't reintroduce a non-optional `to` field.

## Forwarded-attachment paths break when the source email is archived

`create_forward_draft` resolves attachment paths to absolute paths at draft creation time (`src/draft.rs:262-278`). If the source email is then moved from `inbox/` to `archive/`, the hardcoded paths in the draft frontmatter become stale and `send` fails with "Failed to read attachment". Tracked as an open ticket; do not silently break this when refactoring draft creation.

## `cargo install --path .` is required after every change

The user's `email` binary is the installed one, not `target/debug/email`. A code change that builds and tests green is invisible to the running TUI / CLI until you reinstall. This was historically gated behind a codesign script (`./scripts/install.sh`) for keychain ACL stability; the encrypted-file secrets backend removed that need, so plain `cargo install --path .` is canonical again.

## `mailbox_states` must persist to disk, not live only in memory

`AccountState.mailbox_states` (per-role `uid_validity` / `uid_next` /
`exists`) was originally rebuilt every TUI launch, so the first quick
sync fell through to a full Message-ID reconcile (~14 s on a busy IMAP
server). The cache is now written to `<account_dir>/mailbox-states.json`
after every successful Fetch / Sync and reloaded in `AccountState::new`,
making the cold-start sync <2 s. A corrupt or missing file degrades
silently to an empty map -- the worst case is one extra full reconcile,
so we never block startup on cache I/O. Implemented in #0002.

## Adding new files in dotfiles requires a restow

Fish functions, Claude commands/skills, and similar live in `~/dotfiles/<package>/` and are linked into place by GNU Stow. After adding a *new file* to a stow package, run `cd ~/dotfiles && stow -R <package>`. Editing an existing symlinked file is fine -- no restow needed.

## `tokio::runtime::Runtime::new()` panics inside `#[tokio::main]`

Main is `#[tokio::main]`, so a tokio runtime is always live by the time any sync code runs. Calling `tokio::runtime::Runtime::new()` then `rt.block_on(...)` panics with "Cannot start a runtime from within a runtime." Sync sites that need to drive an async future must use `config_cmd::helpers::run_async_blocking`, which detects the existing runtime and spawns a fresh one in a separate OS thread (modelled on `oauth2::load_or_refresh_token_blocking`). The bug went undetected for months because the affected code paths were the wizard's IMAP/SMTP/Graph test calls, which never ran in `cargo test`.

## Attachment paths in drafts must reference the per-account stable mirror

The per-mailbox `<mailbox>/<stem>_attachments/<file>` path follows the email when reconcile detects an inbox -> archive move on the server, so any draft created before the move ends up with stale frontmatter and `mp send` fails with `Failed to read attachment`. Each fetched attachment is therefore also hardlinked into `<account>/attachments/<sanitized-message-id>/` (copy fallback on filesystems that disallow hardlinks). `create_forward_draft` writes those stable paths and lazy-hydrates the mirror for emails fetched before the scheme existed. Cleanup happens in `reconcile_local_files` only when the email vanishes from every synced mailbox -- never on dedup or move (the surviving copy still references the same Message-ID). Helpers live in `src/parse.rs`: `sanitize_message_id_for_path`, `stable_attachments_dir`, `link_or_copy`, `account_dir_for_email`. See ticket #0006.

## Display names with non-atext characters break lettre's `Mailbox` parser

Lettre's `Mailbox::from_str` is a strict RFC 5322 parser: a display name must be either an `atom` (atext only) or a `quoted-string`. Real senders routinely violate this -- TUM mailing lists like `CCBE_Researchers [TUBVCMS] <researchers.ccbe@ed.tum.de>` ship unquoted `[`/`]`, and unquoted commas in `Last, First <addr>` are also common. `send::normalize_address_for_smtp` auto-quotes display names whose characters fall outside RFC 5322 atext + FWS + `.`, escaping inner `\` and `"` per the quoted-string rules. Bare addresses, atext-only names, and already-quoted names pass through unchanged. Tests live in `src/send.rs` under `normalize_*`.

`send_email` parses each address **twice** for different purposes and both sites must use the normalizer:

1. Once per role to build the message *headers* via `Message::builder().to(mbox)/.cc(mbox)/.bcc(mbox)`, plus `from` and `reply_to`.
2. Once per recipient inside the per-recipient `RCPT TO` loop to extract `mbox.email` for the SMTP `Envelope`.

`validate_draft` does the same parse a third time for pre-flight checks. Forgetting any one of them produces a confusing failure mode: validation passes, headers build cleanly, but the offending recipient silently fails at envelope time and the TUI surfaces only `Partial: N/M succeeded` -- the actual `Invalid address '...': Invalid input` line lives in the daily log under `<data_dir>/logs/`. The first fix only patched validation + header build; the envelope loop was caught by a follow-up bug report. Regression test: `normalize_extracts_email_via_mailbox_for_envelope`.

## CSP for saved HTML must allow `file:` images because `cid:` is rewritten at save time

Saved `.html` companions get a `<meta http-equiv="Content-Security-Policy">` tag injected at save time (`inject_csp_meta` in `src/parse.rs`) to block scripts and remote tracking pixels when the file is opened in a browser. The obvious policy `img-src data: cid:` silently breaks inline images: `rewrite_cid_references` runs *before* the file is written and replaces every `cid:` URL with an absolute `file://` path to the extracted attachment, so by the time the browser evaluates the CSP there are no `cid:` URLs left. The policy therefore includes `file:` in `img-src`. This is safe because `script-src 'none'; connect-src 'none'` leaves no exfiltration channel -- a hostile email can at most *display* a local file it already knows the path of, not send it anywhere.

## Never find an HTML injection point by searching for `<head>` in attacker-controlled HTML

The first version of `inject_csp_meta` located the insertion point via `lower.find("<head>")`. A hostile email can hide an earlier `<head>` inside a comment (`<!--<head>-->`) or an attribute value (`data-x="<head>"`), so the security-critical tag lands where the browser never parses it as head content -- the CSP is silently neutralized. The robust zero-dependency fix is to *prepend* the `<meta>` at the very start of the document (after a leading doctype, to preserve standards mode): per the HTML tree-construction spec, a `<meta>` seen before any `<head>`/`<body>` start tag is hoisted into the implicitly created `<head>` before any attacker-controlled bytes are parsed, and a later explicit `<head>` start tag is ignored. Searching remains acceptable only where a spoofed match is harmless (e.g. `ensure_utf8_charset`, worst case mojibake).

## `to_lowercase()` byte offsets are invalid in the original string

`str::to_lowercase()` can change byte length -- 'İ' (U+0130, 2 bytes) lowercases to "i\u{307}" (3 bytes) -- so a byte offset found in the lowercased copy misaligns in the original: wrong insertion point at best, panic on a non-char-boundary slice at worst (`byte index N is not a char boundary`). On attacker-controlled input (email HTML) that panic is a remote sync-crash DoS. For case-insensitive substring search that yields offsets valid in the source string, compare byte windows with `eq_ignore_ascii_case` (see `find_ascii_ci` in `src/parse.rs`) -- never lowercase the whole string for offset math.

## Sync "did anything change?" cannot be derived from saved/moved/removed counts

`SyncResult.saved/moved/removed` undercount local mutations: read-status reconciliation rewrites `read:` frontmatter without touching any of them (`read_updated` is separate), and the CLI dedup pass deletes files counted only in `deduped`. The TUI's post-fetch cache invalidation therefore keys off `SyncResult.touched_dirs` -- the set of local mailbox directories the sync actually modified -- which every mutation site in `sync_mailboxes` (IMAP) and `sync_mailboxes_graph` must insert into: email saves, read-flag updates, dedup, and reconciliation (a move inserts *both* source and destination). If you add a new mutation to either orchestrator, add its directory to `touched_dirs` or the TUI will keep serving a stale cache for that mailbox. Error paths that may have partially written (Graph save/read-status failures) insert conservatively; a failed fetch sends `touched_dirs: None`, which the handler treats as "unknown -- invalidate everything".

## Background mailbox loads need a generation counter, not just index checks

Moving `load_emails` off the UI thread (P1 step 2) looks like a pure "compare account/mailbox indices on arrival" problem, but two subtler races make index equality insufficient. (1) **Optimistic mutations**: archive/delete remove the entry from `app.emails` *before* the server confirms; a directory walk that started before the file actually moved would resurrect the removed email when its result lands -- indices still match, so only a generation bump in `remove_selected_from_list{,_batch}` catches it. (2) **Reload storms**: successive reloads of the *same* mailbox (fetch result + editor return) can complete out of order; the older walk's snapshot must not clobber the newer one. `App::mailbox_load_generation` is bumped on every `request_mailbox_load` and every optimistic mutation; `BgResult::MailboxLoaded` is applied only when indices *and* generation match (`mailbox_loaded_is_current` in `src/tui/bg.rs`). Stale results also must NOT populate `email_cache` -- the slot stays `None` so the next visit reloads. Related ordering trap: in `Action::EditCurrent` the auto `MarkAsRead` must be queued *before* `reload_current_mailbox`, otherwise the background walk can read the file before the read-flag write and the fresh list briefly shows it unread again.

## Arc-cached email lists: release the cache slot's strong ref before `Arc::make_mut`

The P2 refactor shares one `Arc<Vec<EmailEntry>>` between `App::emails` and the active `email_cache` slot. The naive mutation pattern -- `Arc::make_mut(&mut self.emails)` while the slot still holds a clone -- deep-copies the whole Vec on *every* mutation, because the strong count is always ≥ 2. `App::with_emails_mut` therefore takes the slot out first (dropping its strong ref), runs `Arc::make_mut` (now usually strong count 1 → in-place mutation), and puts a fresh `Arc::clone` back afterwards. A residual copy still happens right after `save_to_account` mirrors the cache into `AccountState` (strong count 2 via the mirror), which is at most one clone per account switch -- acceptable. Two invariant traps: (1) if the slot was `None` (invalidated / background load in flight) it must STAY `None`, otherwise a stale in-memory list gets promoted back into the cache; (2) `App::visible` (the filtered index view) holds indices into `emails`, so every structural mutation or reassignment of `emails` must be followed by `rebuild_visible()` or the view dangles -- `selected_email()` guards with `.get()` but the cursor/row mapping would silently go wrong.

## Bidirectional read-status sync: server snapshots go stale in flight, and coverage windows must not shrink

Ticket #0004 ("read status resets after fetch; \Seen sync unreliable") turned out to be three independent bugs. (1) **Snapshot clobber**: sync captures server `\Seen`/`isRead` flags in pass 1, then applies them to local frontmatter seconds later; any local mark made *during* that window (auto-mark-on-preview, `m` -- exactly what a user does while a startup auto-fetch or IDLE-triggered sync runs) was silently reverted to the older server state. The fix is a snapshot-staleness guard, **not** a 3-way merge: `sync_local_read_flags{,_with_index}` takes a `snapshot_cutoff` captured *before* the server read, and skips any file whose mtime is at-or-after `cutoff - 1s` (slack for coarse filesystem mtime granularity; erring toward skipping is safe, the file just converges next sync). The skipped file's own local→server propagation is already in flight, so state converges. If both-sides-changed-while-the-app-was-closed conflicts ever matter, the upgrade path is tracking last-synced state per message (true 3-way merge) -- deliberately not built now to avoid a new persistent format. (2) **Probe fast path shrank flag coverage to 10 messages**: the adaptive probe returned early when the newest 10 UIDs were all known, so with no new mail (the common case) webmail read/unread changes on anything older never reached local files. Pass 1 header+FLAGS over the full 100-message window costs ~10 KB; the probe's saving was one small FETCH, so it was removed rather than patched -- if you reintroduce a probe, it may only skip pass **2** (bodies), never pass 1's flag collection. (3) **Graph path substring-matched read state**: `content.contains("read: true")` matched body text too (any email *quoting* that string could never sync unread→read) and diverged from the IMAP path's frontmatter parser; both backends now share the same frontmatter-aware, cutoff-guarded helper. Regression seam: `sync_local_read_flags` is the single choke point both orchestrators feed (IMAP pass-1 flags, Graph `fetch_message_ids`), so `tests/sync_integration.rs` covers both backends by testing it directly.

## In-place YAML frontmatter rewriting (2026-07-11)

Rewriting individual frontmatter keys in place (`rewrite_draft_recipients`, `src/draft.rs`) is subtler than line replacement: a key's value can be a block scalar (`>`/`|`) whose continuation lines are indented AND may contain interior blank lines — all of it must be consumed when replacing the key, or the leftovers make the whole frontmatter unparseable. Match managed keys at zero indentation only (nested keys under other mappings must not match), and write via temp-sibling + rename (`write_atomic`) so a mid-write failure never truncates the draft. Two adversarial review rounds were needed to get this right; the regression tests in `src/draft.rs` (folded/literal/interior-blank/CRLF/nested-key) are the spec.

## iMIP CANCEL/update: identity is `(UID, RECURRENCE-ID)`, the version chain is `(SEQUENCE, DTSTAMP)`, and neither is a delete (2026-08-11)

Ticket #0031, receive side. Five traps, all of them cheap to get wrong and expensive to notice.

(1) **A CANCEL is a tombstone, not a delete.** Deleting the row would destroy the only local record of a meeting the user may still need to reason about ("what was that thing that got called off?"), and the message is a real mail row anyway, so a delete would fight the store's server-as-truth rule. The event stays listed with a banner and a badge; nothing on disk moves.

(2) **`>=`, not `>`, for the CANCEL sequence.** RFC 5546 says a CANCEL bumps `SEQUENCE`, but Outlook regularly emits a CANCEL at the same sequence as the REQUEST it kills. A strict `>` silently ignores those. The mirror-image trap is the *stale* CANCEL: a cancellation below the surviving REQUEST's sequence cancelled a version that was already replaced (organizer cancels, then reschedules), and applying it would strike through a live meeting.

(3) **Supersession must be strict.** `latest > (seq, dtstamp)` with a *strict* compare is what makes a re-delivered copy of the current version (same sequence, same DTSTAMP, second mailbox) not mark itself superseded. An `>=` there flags every event whose copies collapsed during dedup.

(4) **`RECURRENCE-ID` scopes everything.** A CANCEL carrying one addresses a single occurrence; folding it under the bare UID tombstones the whole series, which is the single most destructive-looking bug on this surface. Normalise it through the *same* function as `DTSTART` (`DatePerhapsTime::from_property` + `format_date_perhaps_time`), so `RECURRENCE-ID:20260720T120000Z` and `RECURRENCE-ID;TZID=Europe/Berlin:20260720T140000` are one identity; inventing a second timezone path here would have made the two forms two different occurrences.

(5) **Derive, never persist.** The cancellation/version fold is computed from the account's ics blobs on every read, like the REPLY fold before it (#0030). That is what makes arrival order irrelevant -- a CANCEL ingested before its REQUEST is just another row when the fold runs -- and it means there is no stored flag to go stale when the store is rebuilt. The consequence worth stating: RSVP guards must read the *folded* event, not the parsed payload under the cursor, or `V` will happily reply to a cancelled version.

## iMIP REPLY: echo UID/SEQUENCE from the sidecar, not the frontmatter (2026-07-12)

When building an attendee RSVP (`METHOD:REPLY`, ticket #0029), the `UID` and `SEQUENCE` must be copied verbatim from the received invite's sidecar `invite.ics`, never from the `event:` frontmatter cache — the frontmatter is a lossy render/query mirror and can drift, but the organizer's client threads the reply by exact `UID`+`SEQUENCE`. `reply_context_from_ics` (`src/invite.rs`) re-parses the raw `.ics` for this reason. Two more non-obvious REPLY details bit-for-bit matter: (1) a REPLY `ATTENDEE` carries **no** `RSVP` parameter (that lives only on the REQUEST), and (2) `RECURRENCE-ID` is intentionally absent because v1 answers the whole series only (D6) — adding it would scope the reply to a single occurrence. The `icalendar` crate reorders VEVENT properties alphabetically on serialization, so assert on unfolded substring presence, not line order. Local state (`event.rsvp`) is flipped only *after* a successful send, and only via the in-place YAML rewrite (`set_event_rsvp`) that touches the single `rsvp:` line under `event:` and leaves the sidecar and body byte-for-byte intact.

## Organizer-side REPLY reconciliation: surgical nested-sequence rewrite, and idempotency as a byte invariant (2026-07-12)

Reconciling attendee RSVPs into a sent invite (`set_event_attendee_status`, `src/draft.rs`; `src/reconcile.rs`, ticket #0030) rewrites a `status:` line *inside* the `attendees:` block sequence under `event:` — one level deeper than `set_event_rsvp`'s direct child. The nested walk has three traps a naive line-replacer falls into: (1) YAML allows a block-sequence item (`- address:`) to sit at the **same** indentation as its parent key (`attendees:`), so "stop when indent ≤ parent" ends the sequence before it starts — the loop must treat `- ` item lines specially and only terminate on a *non-item* line at/shallower than `attendees:`; (2) when `status:` is the item's inline first key (`- status: ...`), preserving the literal prefix `  - ` (not `"    "` spaces) is what keeps the dash — reconstruct the replacement from `line[..key_col]`, never from `" ".repeat(col)`; (3) a `description:`/`notes:` block scalar can contain lines that look exactly like `- address:`/`status:` mapping entries — the rewriter must never touch them, which falls out for free from only scanning inside the real `attendees:` sub-block. Chose a line-surgical rewrite over a serde re-serialize of just the `event:` block because serde_yaml reflows quoting/indent/key-order and would not round-trip block scalars or CRLF byte-for-byte. **Idempotency is a byte invariant, not a logical one**: `set_event_attendee_status` returns `Unchanged` and skips the write when the target status already matches, so a second `reconcile_account` produces an identical file — the integration test asserts `fs::read` equality across two runs, which is the real contract for "two machines rebuild identical derived state from IMAP alone." Latest-reply-wins is keyed on `(SEQUENCE, DTSTAMP)` (a tuple compare, DTSTAMP as an RFC3339-UTC string that sorts chronologically); replies whose sequence is older than the invite's are dropped. `parse_ics` gained `dtstamp` (via `icalendar::Component::get_timestamp`) purely for this tiebreak. One gotcha unrelated to the feature: test fixtures with unquoted `subject: Re: Plan` fail to deserialize (`mapping values are not allowed`) because the second colon starts a nested mapping — quote any header value containing `: `.

## iMIP REPLY must carry DTSTART for Exchange, and cloning the parsed Property preserves the datetime form (2026-07-12)

RFC 5546 lists `DTSTART` as OPTIONAL in a `METHOD:REPLY`, but Exchange/Outlook reject a reply that omits it: the RSVP arrives as an unusable `not supported calendar message.ics` with "Invalid ICAL element: DTSTART" (live smoke-test failure on ticket #0029, shipped in e058e46). Outlook itself echoes the invite's `DTSTART`/`DTEND` back in its own replies, so `build_reply_ics` (`src/invite.rs`) now does the same. The form-preservation trick: rather than re-parse the invite datetime into a `chrono` value and re-emit it (which would flatten a `TZID`/wall-clock invite to UTC `Z` and drop `VALUE=DATE`), read the already-parsed `icalendar::Property` straight out of the source event's `properties()` map (`event.properties().get("DTSTART").cloned()`) and `append_property` it verbatim into the reply. `Property` is `Clone` and carries both the value and its parameters (`TZID`, `VALUE=DATE`, ...), so a UTC-`Z`, a `TZID=Europe/Berlin` wall-clock, and an all-day `VALUE=DATE` invite all round-trip byte-identically with zero datetime math on our side. Carry `DTEND` when present, else `DURATION` (mutually exclusive in a well-formed invite). Defensive: if the invite genuinely has no `DTSTART`, build the reply without it and `log::warn!` rather than failing — a start-less REPLY is still better than no RSVP. The crate serializes VEVENT properties alphabetically, so assert on unfolded substring presence, not order. Everything else about the reply (single ATTENDEE + PARTSTAT, fresh DTSTAMP, verbatim UID/SEQUENCE, echoed ORGANIZER/SUMMARY) is unchanged; PRODID stays the crate default `ICALENDAR-RS` (a single valid PRODID, fine for Exchange).

## Off-Mail pane keys must beat swallowed Global keys, not lose to them (2026-07-20)

The multi-view dispatcher (#0033) swallows mail-specific Global keys when the active view is not Mail (`is_view_agnostic()` gate in `dispatch_normal_mode`, `src/tui/app/keys.rs`) so e.g. `1-9`/`/` do nothing in Contacts/Calendar. But Global resolves *before* the pane context, so a naive "resolve Global → if not view-agnostic, swallow and return" silently steals any key a non-Mail pane wants to rebind — the Contacts fuzzy-search `/` never fired because Global's `/` (FilterMetadata) was swallowed first. Fix: before swallowing a non-view-agnostic Global hit off-Mail, check whether the active view's pane context *rebinds* that same key (`resolve(pane_ctx, ...)`); if it does, fall through to the pane binding instead of returning. Only truly unclaimed mail Global keys are swallowed. The hint bar mirrors this: off-Mail, `render_hint_bar` filters the Global row to `action.is_view_agnostic()` so it never advertises swallowed keys (`1-9 Jump to mailbox`, `/ Filter by metadata`) that the pane may re-own. Regression seam: `contacts_fuzzy_search_filters_list` presses `/` in Contacts and asserts the search input arms — it fails against the swallow-first ordering.

## Compose-wizard/search inputs are append-only; horizontal scroll must keep the tail visible, width-aware (2026-07-29)

The TUI's single-line inputs (compose To/Cc/Bcc/Subject, list `/` search, server search, Contacts fuzzy search, dir/mailbox pickers) have **no cursor column** — the handlers only `push`/`pop`, so the caret is always at the end of the string (`src/tui/app/keys.rs`). They also rendered the whole value from the left with no scrolling, so once a field grew past its width the caret and new text walked off the right edge and became invisible while still working (TKT-0046: "I can't see the email address I am adding past one or two"). Because the model is append-only, keeping the *end* visible is sufficient — no mid-text cursor math needed. The shared fix is `visible_window(text, cursor_char, width)` + `scrolled_input_value` in `src/tui/ui/util.rs`: it walks a per-char display-width prefix table and picks the largest tail that fits `width` cells, returning the slice + `clipped_left`/`clipped_right`/`cursor_col`. Two traps: (1) width must be measured with `unicode-width` (`UnicodeWidthChar::width`, control chars → 0), never `chars().count()` — CJK/wide chars are 2 cells, so a char-count budget overshoots and re-introduces the very overflow you're fixing (the repo's older `truncate()` still counts chars, noted in `docs/herdr-tui-inspiration.md`); (2) slice by collecting `chars[start..end]`, never byte ranges, or a multi-byte umlaut boundary panics. Each caller subtracts its own prompt width and reserves one cell for the block-cursor glyph before calling the helper, and prepends `…` when `clipped_left`. `unicode-width` was already a transitive dep; promoted to a direct dependency. Unit tests in `util.rs` cover ASCII fit/scroll, mid-text cursor, umlaut and CJK no-panic + width-bounded window, empty, and zero-width.

## `account_dir(name)` with an empty name is the shared accounts root, not an empty path (2026-07-29)

`config::account_dir("")` returns `<data_dir>/accounts/` — the parent of *every* account. Any feature that walks an account root from `App::account_config.name` (the Calendar agenda loader, #0034) must therefore guard on a non-empty name, or it silently walks the whole mailstore: every account's events land in one agenda, and unit tests built on `App::default_for_tests` (which leaves `account_config` at `Default`, i.e. `name: ""`) stop being hermetic and start reading the developer's real `~/.local/share/mailypoppins`. The symptom is a test that passes on CI and fails locally (or vice versa) with timing/count noise rather than an obvious path error. Guard at the single point where the root is derived (`App::calendar_account_root`), not at each call site.

## ratatui's `Wrap { trim: true }` is a word wrapper, so a cell-count `div_ceil` under-reserves rows (2026-07-29)

Reserving `display_width(text).div_ceil(width)` rows for a wrapped `Paragraph` is the *character-packing* lower bound, not the height ratatui actually needs: `WordWrapper` breaks on whitespace, so every word that straddles a boundary costs an extra row. The Calendar pane's Outlook caveat (#0034) was clipped at 16 of the 231 terminal widths sampled, and the truncation ate the operative clause (`Graph sync (#0036)` gone at 164 columns, everything after `need` gone at 113). The fix is a small greedy word-wrap counter mirroring `WordWrapper` (`wrapped_rows` in `src/tui/ui/calendar.rs`): pack words separated by one space, and break a word wider than the pane across rows. Two test traps this hid behind: (1) a unit test asserting the reserve formula against itself (`caveat_rows(30, 20) == cells.div_ceil(30)`) is tautological and locks the bug in — assert against *rendered* rows via `TestBackend` instead; (2) when flattening a `TestBackend` buffer to search for text, iterate the block's **inner** area, not the whole frame, or the box-drawing border glyphs splice into the string and every `contains` fails for the wrong reason.

## `.md` files under an account root are not all our mail: attachment dirs are sender-controlled (2026-07-29)

`parse.rs` writes inbound attachments to `<mailbox>/<stem>_attachments/<name>.md` and mirrors them to `<account>/attachments/<message-id>/<name>.md`, and `sanitize_attachment_filename` preserves the `.md` extension. Any unbounded `WalkDir` over the account root that trusts `*.md` frontmatter is therefore parsing attacker-chosen content: an attached `.md` with an `event:` block and a real invite's UID plus `sequence: 4294967295` displaces the genuine agenda row (attacker summary, organizer and start time), and the same trick with `method: CANCEL` strikes a real meeting through. The live mailstore already holds 72 attachment `.md` files. The Calendar loader now skips any path with a component equal to `attachments` or ending in `_attachments` (`is_attachment_path`, `src/tui/app/calendar_view.rs`); the invite's own `invite.ics` sidecar still lives in such a dir and is still read, but only through `authoritative_ids`, keyed off a real email's path. `reconcile::build_index` has the same unbounded walk and is tracked as TKT-0047. Every other body-reading walk in the repo is already `max_depth(1)`.

## IMAP `HEADER` is a substring match, so a Message-ID lookup needs a client-side exactness pass (2026-07-29)

RFC 3501 defines `HEADER <field> <string>` as "contains the specified string in the text of the header", not equality, so `HEADER "Message-ID" "abc@x"` also returns `<prefix-abc@x>` and `<abc@x.evil.net>`.
Two things make the `message-id:` search prefix (TECHLEV-6) exact.
First, the query always goes out angle-bracketed (`bracketed_message_id`), because the brackets are what pin both ends of the identifier inside the header text.
Second, `retain_exact_message_id` re-checks equality on the parsed results, which is also what catches the Graph backend falling back from `$filter` to a fuzzy `$search` when `internetMessageId eq` is rejected.
The filter lives in `fetch_emails_on_session`, the single seam every IMAP search path goes through (CLI, TUI `f` overlay), rather than at the call sites.
Comparison is `eq_ignore_ascii_case` on the bracket-stripped value: servers are inconsistent about the domain part's casing, and this is still equality, never a substring.

## The TUI cursor is a bare index, so any list rebuild silently moves it (2026-07-30)

`App::list_index` indexes `visible`, which indexes `emails`, and none of those three levels carries the identity of the row the user is looking at.
Every reload therefore had to "preserve" the selection with `list_index.min(visible.len() - 1)`, a clamp that only guarantees the index is in range, not that it still points at the same email.
Two everyday operations break it: approving a draft rewrites `status:`/sort key so the list re-sorts, and new inbox mail prepends a row so everything shifts down one under queued keystrokes.
The fix is to anchor on `EmailEntry::path`, the de-facto stable key the codebase already keys `selection` and `set_email_read` on: `cursor_anchor()` before the rebuild, `restore_cursor(anchor, fallback)` after, with the old clamp as the fallback when the anchored email is genuinely gone.
`BgResult::MailboxLoaded` (`src/tui/bg.rs`) is the single funnel every async reload passes through, so anchoring there covers `reload_current_mailbox`, watcher-triggered fetches and sync arrivals at once.
Two site-specific traps: in `switch_account` the anchor captured before the swap belongs to the *outgoing* account and can never match, so the incoming account's path is parked in `AccountState::cursor_path` by `save_to_account` instead; and in the batch-removal path the fallback must be the number of *surviving* rows above the old cursor, or archiving rows above the cursor drags it downward.
Intentional resets (`apply_search_filter`, `reload_from_cache`, a real mailbox change) still go to row 0 by design.

## Status transitions must be line surgery, not a frontmatter round-trip (2026-07-30)

`mark_as_approved` / `mark_as_draft` / `update_status_to_sent` used to `parse_email_draft` into `EmailFrontmatter`, flip one field, and `serde_yaml::to_string` the struct back.
`EmailFrontmatter` has no `date` field, so every approve silently deleted the `date:` line (plus any user-added key) and added `cc: null`/`bcc: null` noise.
The TUI's `resolve_date` then fell back to `sent_at` (null) and finally to the filename, which for a TUI-created `draft-%Y%m%d-%H%M%S.md` does not parse, so `date_sort` became empty and the approved row teleported to the bottom of the list.
Any writer that re-serializes a partial view of a document is lossy by construction; the repo already had the right pattern in `rewrite_draft_recipients` and `set_event_rsvp`.
`rewrite_frontmatter_scalars` (`src/draft.rs`) now backs all three, replacing only the named top-level key lines and appending absent ones before the closing fence.
`update_status_to_sent` keeps a serde fallback for exactly one caller: `mp invite` builds a synthetic `EmailDraft` that was never written to disk, so there are no source bytes to preserve.
The regression seam is a byte-equality assertion (`after == before.replace("status: draft", "status: approved")`), which catches field loss and reflowed quoting that a struct-level assertion cannot see.

## An atomic rename replaces the inode, so it silently resets the target's permissions (2026-07-31)

`write_atomic` (`src/draft.rs`) creates a temp sibling and renames it over the destination, which is what makes the overwrite atomic, but the renamed file carries its *own* inode and therefore its own mode: a draft the user had chmod'ed to 0600 came back 0644 (umask default) after every approve, demote or mark-as-sent.
The fix copies the existing target's mode onto the temp file *before* the payload is written, not after, so the content is never briefly on disk under wider permissions than the user asked for.
A target that does not exist is left alone, so a newly created draft still follows the umask rather than inheriting some arbitrary earlier mode.
The same trap applies to any copy-and-rename writer; `secrets::write_secret_file_atomic` dodges it only because it hardcodes 0600 on the temp file.

## `mailparse` decodes 8-bit header bytes two different ways (2026-08-05)

Bytes 0x80..0x9F reach the user as different characters depending on where they sit in the message.
Inside an `=?ISO-8859-1?Q?...?=` encoded-word, and inside a body declared `charset=iso-8859-1`, they decode as windows-1252, so 0x93/0x94 become the curly quotes the sender meant; that mapping is what the WHATWG encoding standard and every browser do with the `ISO-8859-1` label.
Raw in a header with no encoded-word at all, they decode as strict ISO-8859-1 instead, so the same bytes land as the invisible C1 control characters U+0093/U+0094 and get written straight into the frontmatter.
Charset decoding is otherwise solid on the receive path: ISO-8859-1, windows-1252 and Shift_JIS bodies, quoted-printable included, all come out right, and a latin-1 HTML body is transcoded to UTF-8 with its stale `<meta charset>` replaced.
The oracles are in [tests/mime_oracle_integration.rs](../tests/mime_oracle_integration.rs), tagged `parity` or `known-bug` per [#0049](tickets/0049-pre-nuke-oracle-capture.md).

## clap's `--help` is not always the long help (2026-08-05)

Snapshotting the CLI surface in-process with `Cli::command().render_long_help()` records a layout no user ever sees.
clap only wires `--help` to the long help when the command actually has long help somewhere; `mp send --help` and `mp send -h` are byte-identical compact output, while `mp dump-keys --help` differs from `-h` because that one carries a multi-paragraph doc comment.
The in-process route also needs `.bin_name("mp")` (clap otherwise prints the `#[command(name)]`, "mailypoppins") and a `cmd.build()` to propagate that name into nested usage lines.
Running the built binary through `env!("CARGO_BIN_EXE_mp")` avoids all three traps, needs no dependency, and costs about 0.25s for the whole 38-screen walk.

## `attachments:` holds source paths on the send side (2026-08-05)

The frontmatter key means two different things depending on which way the mail went.
On received mail it lists the file names saved into the sibling `<stem>_attachments/` directory, but on drafts and sent mail it holds whatever the sender typed, which is the *source path* of the file to attach: the real tree has entries like `/tmp/audio-scripts/sql-kg-rag/01-segment1-introduction.mp3` and one under `/home/sylvain`.
Anything that treats the list as a set of names therefore leaks absolute paths, and any size lookup that joins the entry onto a directory silently resolves against the filesystem root instead (`Path::join` with an absolute argument discards the base).
`mp dump-mailbox` reduces each entry to its file name before both the output and the size lookup ([src/dump.rs](../src/dump.rs)); the same normalisation is what the SQLite store will need to reproduce the dump.

## FTS5 external content silently accepts a column the content table lacks (2026-08-06)

The store's schema sketch declares `messages_fts USING fts5(subject, from_, body_text, content='messages')`, but `messages` has no `body_text` column because the body lives in a blob.
`CREATE VIRTUAL TABLE` accepts that without complaint, and so do explicit `INSERT`s into the index and `MATCH` queries that only return `rowid`: FTS5 resolves content columns lazily, at the moment a query actually needs a column *value*.
The failure surfaces later as `no such column: T.body_text` from `snippet()`, `highlight()`, `SELECT subject FROM messages_fts` and the `INSERT INTO messages_fts(messages_fts) VALUES('rebuild')` command.
So the search path must join the matched rowid back to `messages` for anything it wants to display, and the index has to be written by ingest rather than rebuilt from the content table.
That is workable here only because a store that loses its index is dropped and refilled by the next sync ([src/store/schema.rs](../src/store/schema.rs)).

## A blob refcount that unlinks inside a transaction can outlive its rollback (2026-08-06)

`BlobStore::release` ([src/store/blobs.rs](../src/store/blobs.rs)) is handed the caller's connection so the decrement commits with the row that dropped the reference, but the `unlink` at refcount zero is a filesystem call and does not roll back with it.
A caller that releases and then rolls back therefore keeps a row whose `body_blob` names a file that is already gone.
That direction is survivable because the server is truth (a missing blob reads as evicted and is re-fetched), while the opposite direction is not: a committed row pointing at bytes that were never written is a hole in the read path.
Same reasoning drives the write order on the ingest side, where the blob file is written *before* the transaction that acquires the reference, so a crash leaves an unreferenced orphan rather than a dangling hash.
The temp file also lives in the destination fan-out directory (`blobs/ab/cd/.<hash>.tmp.<pid>.<n>`), not in a shared temp root, so the rename stays on one filesystem and an interrupted write is invisible to both `contains` and `read`.

## FTS5 external-content: delete before you release the blob you deleted from

`messages_fts` is external-content over a `messages` table that has no
`body_text` column, so `'rebuild'` and a plain `DELETE FROM messages_fts` both
fail and the only way to remove an entry is the FTS `'delete'` command with the
*original* column values.
On re-ingest those values include the old body text, which lives in the old body
blob, and `BlobStore::release` unlinks that blob the instant its refcount hits
zero.
Releasing the old references before issuing the `'delete'` therefore makes the
old body unreadable and leaves a stale FTS entry behind that no later write can
remove (#0037 unit 4a).
Order is: write the row, delete the old FTS entry, re-point the blob references,
insert the new FTS entry.

## The outbox holds a plain blob reference, and a failed row keeps its bytes

An `outbox` row's `raw_blob` is acquired through `BlobStore::acquire` with **no** `message_blobs` row, because that table's foreign key targets `messages` and a submission is not a message (#0037 unit 4b).
The reference is released when the row reaches `done`, inside the same transaction, and on an explicit `outbox::discard`.
It is deliberately *not* released on the transition into `failed`: that state exists so a human can read the message SMTP may or may not have delivered, and releasing would unlink exactly those bytes.
Completion also ingests the sent copy *before* releasing, so the raw hash passes from the outbox reference to the message's own reference without touching zero (`release` unlinks the instant it does).

## async-imap 0.11 discards APPENDUID

`Session::append` returns `Result<()>` and its tagged-`OK` response code, where `APPENDUID` lives, is dropped inside the crate's private `check_done_ok`; the stream is private too, so the command cannot be re-run by hand from outside.
What survives is still a definitive acknowledgement, because `async-imap` turns any non-`OK` tagged response into an error, so a successful return means the server filed the message.
`ImapSentMailbox::append` ([src/imap_client/sent.rs](../src/imap_client/sent.rs)) therefore recovers the UID with the same `UID SEARCH HEADER MESSAGE-ID` the dedup path runs and stores it on the row.
Reading the real `APPENDUID` needs a patched or vendored `async-imap`; the `SentMailbox` seam is where that would land.

## lettre drops the Bcc header, so the envelope has to be stored separately

`Message::builder().bcc(...)` puts the address in the envelope and then removes the `Bcc` header from the built message, unless `keep_bcc()` is called (lettre 0.11, `message/mod.rs`).
That is correct for a blind copy and fatal for anything that tries to reconstruct a submission from the message bytes: the blind recipients are simply not in there.
The durable outbox therefore stores an `envelope` column (`from:` plus one `to:`/`cc:`/`bcc:` line per recipient) alongside the raw blob, and a resumed send addresses from that, never from the headers (#0037 review).

## A schema amended in place needs a column check, not just a table check

The store has no migrator: a version mismatch or a missing table drops and rebuilds the file.
A column added to an existing version is invisible to both checks, so a store written by an earlier build of the same version passes validation and then fails every write against the new column.
`schema::REQUIRED_COLUMNS` exists for exactly that window and is checked beside `REQUIRED_TABLES` on open; it is emptied again whenever a change bumps `SCHEMA_VERSION` (#0037 review).

## An external-content FTS5 index cannot be corrected without the old row values

`messages_fts` was declared `content='messages'` over a `messages` table that has no `body_text` column, so the index was written by hand and the only way to undo an entry was the `'delete'` command replaying the *old* subject, from and body.
Re-ingest read the old body back from its blob, and a blob the retention sweep had already evicted left it with nothing honest to say: it skipped the delete and the row stayed indexed twice (#0037 known issue).
`content=''` with `contentless_delete=1` (FTS5, SQLite 3.43+; the bundled build is 3.46) makes `DELETE FROM messages_fts WHERE rowid = ?` legal with no column values at all, which removes the dependency instead of working around it.
Nothing else changes: a contentless index was already the only thing the code could use, since `snippet()`, `highlight()` and column reads never worked on the external-content declaration either (#0038 unit B).

## The blob store is content-addressed, so two identical bodies are one file

A test that evicts "one message's body" by unlinking its blob evicts every message that happens to carry the same bytes, because the hash is the filename and the refcount is per hash.
Fixtures that need one readable and one evicted body must give the two messages different text (#0038 unit B).

## Our own RSVP needs no stored column, because the sent copy lands during the send

`outbox::ingest_sent_copy` runs inline from `send_durably` -> `settle()`, so the `METHOD:REPLY` we just emailed is already a row in the store's `sent` mailbox by the time the send returns.
Our own `PARTSTAT` is therefore derivable immediately from the same blobs every other attendee's status comes from, with no sync lag, no extra column and no write against the invitation.
That is what makes derive-on-read viable for the whole fold: [src/reconcile.rs](../src/reconcile.rs) writes nothing, and `own_rsvp` falls back to whatever `PARTSTAT` the organizer's own REQUEST carries for us, which is the same fact from the other direction once they have processed the reply (#0038 unit C).

## The invite badge is a listing-query column; the event card is a lazy parse

Two things need to know about an iMIP payload and they have opposite shapes.
The list badge needs an answer for every row of the mailbox, so it rides on the listing query as an `EXISTS (SELECT 1 FROM message_blobs ...)` column (`MessageRow.is_invite`) and costs no blob read at all.
The preview event card needs a fully parsed event for exactly one row, so it is read and parsed on demand and memoised beside the body in `PreviewInvite`, keyed by `(account, MessageRef, generation)` like `PreviewBody`.
Parsing every row's ics to fill an `Option<EventFrontmatter>` on the entry, which is what the pre-store build effectively did through frontmatter, would put back the per-row blob read the lazy body work had just removed (#0038 units B and C).

## A self-invited event ties on `(sequence, dtstamp)`, so the sent copy must win explicitly

One event usually exists as several rows: the copy in `sent`, the copy the server dropped in `inbox`, and any archived copy.
They are deduped by iCal UID keeping the highest `(sequence, dtstamp)`, but a self-invited event shares one `DTSTAMP` across every copy, so that comparison ties and the winner falls to the next component.
With a plain identity tiebreak the winner is whichever mailbox name sorts last, and `is_organizer` flips off for any custom mailbox sorting after `sent` (`team` beat `sent`).
The rank is therefore `(sequence, dtstamp, is_organizer, mailbox, uid)`, with the organizer flag load-bearing above the identity, in [src/tui/app/calendar_view.rs](../src/tui/app/calendar_view.rs) (#0034, carried onto rows in #0038).

## A deleted row's id is handed to the next message, so no reference may cross a delete

`messages.id` is a plain `INTEGER PRIMARY KEY`, which is `rowid`, and SQLite assigns `max(rowid) + 1`.
Delete the row and the number goes back in the pool: the next ingest into an empty table is handed the id the deleted message had, so a `MessageRef` kept across the boundary does not merely miss, it silently names a different message.
Every holder therefore drops it at the mutation: the list, the selection set and the cursor anchor are scrubbed in `App::remove_selected_from_list` and its batch twin, and the hazard itself is pinned by `a_deleted_row_id_can_be_handed_to_the_next_message` in [src/store/write.rs](../src/store/write.rs) (#0038 unit D).

## A moved row cannot keep its uid, because `UNIQUE (account, mailbox, uid)` is the identity

Moving a message locally is an `UPDATE messages SET mailbox = ?`, and the destination may already hold a row under the source's UID: uids are per-mailbox counters, so a collision is ordinary rather than exotic.
The moved row parks on `uid = -id` instead, a value no backend produces (IMAP uids are unsigned and `ingest::graph_uid` clears the sign bit) and unique by construction, which reads as "moved locally, not yet seen there by a sync".
The next sync of the destination finds the row through the `message_id` index and writes the real uid over it, which is the same rebind a UIDVALIDITY reset takes, so nothing extra is needed to converge (#0038 unit D).

## A command that walks a tree the build no longer writes reports success, not absence

`mp open` and `mp save` called `list_attachments` on the `<stem>_attachments/` directory ingest stopped writing, got an empty list, printed "No attachments found" and exited 0, for messages that do have attachments.
An empty walk is indistinguishable from an empty result, so the moment a data source is decommissioned, every reader of it has to decline explicitly rather than be left to discover nothing there.
The rule applied in #0038: path-taking commands fail with the `#0050` boundary line before any filesystem access, which is also what stops `mp reply` and `mp forward` from surfacing a bare I/O error from `parse_email_draft` as if the user had named a bad file.

## A required frontmatter field that our own template leaves blank makes the file invisible

`mp new` wrote the draft skeleton with a bare `subject:` key, which YAML reads as null, and `EmailFrontmatter.subject` was a mandatory `String`.
The draft therefore failed to parse, the drafts index skipped it with a log line nobody reads, and `mp new` printed a selector that `mp path`, `mp edit` and `mp list` could not resolve: the command reported success and produced something unreachable.
The fix has to be both halves, and the halves are not interchangeable.
A file this build writes must parse in this build, so the skeleton writes `subject: ""` rather than leaning on parser leniency; and the field tolerates null and absence, because agents write drafts into `drafts/` by hand and a strict field turns their file into a silent skip instead of a visible error.
`validate_draft` still refuses an empty subject, which is where an unsendable draft should be reported: an index that drops rows is a diagnosis nobody can reach, a validator that says "Missing 'subject' field" is one they can (#0050, [src/types.rs](../src/types.rs) and [src/draft.rs](../src/draft.rs)).

## The Message-ID a header carries is not the key a selector uses

Ingest stores the `Message-ID` header verbatim, angle brackets included, because that is what the wire format says.
The `mp://` selector key is defined as the identifier without them, so `resolve_received` looking the key up as stored matched nothing at all, and `Selector::for_message` would otherwise have printed every selector with a trailing `%3E`.
Delimiters are not part of an identifier: `Selector::for_message` strips them, and the resolver asks for the bracketed form first and the bare one second, so a pasted mail header and a hand-typed key both land on the same row ([src/selector.rs](../src/selector.rs), #0050).

## A guess about what a string is must not run on a string that already said

`selector::parse` refused anything ending in `.md` as a filesystem path, so that `mp archive ./inbox/mail.md` names the real mistake instead of "no match".
The heuristic ran on the whole input, scheme included, which made the canonical form of any key ending in `.md` unparseable: a Message-ID on a `.md` ccTLD, or a draft id ending that way, could be printed by the build and then refused by it.
A heuristic is for ambiguous input only.
Once a string carries its scheme it has declared what it is, and the parser has no business sniffing it ([src/selector.rs](../src/selector.rs), #0050 review).

## A derived index keyed on a field the user controls needs a collision report, not just a winner

The `drafts` table is keyed by `(account, id)` and the reindex upserted, so two files carrying the same `id:` frontmatter collapsed to one row: the losing file stayed on disk and stopped being addressable by any selector, with nothing said anywhere.
The fix is visibility rather than resolution, because nothing but the user can say which file was meant: the reindex picks a deterministic winner (newest file, ties broken by path, so a re-index never flips the answer) and reports the pair, in the log and on `mp`'s stderr, naming both paths ([src/store/drafts.rs](../src/store/drafts.rs), #0050 review).

## A typed key that only one half of the list can produce silently kills the other half's batch

The TUI's multi-select set was keyed on `MessageRef`, the id of a `messages` row, which drafts do not have: `entry_from_draft` leaves `msg` empty and carries the indexed `id:` instead.
`Ctrl+a` therefore selected nothing in the Drafts mailbox, `v` toggled nothing there, and `A` / `D` always fell through to their single-draft path, so `Action::BatchApprove` and `Action::BatchMarkDraft` were reachable by keystroke, present in the keymap and in the help overlay, and unreachable in fact.
Nothing failed: the confirmation dialog never opened, and a flow that does not run reports nothing.
The fix is to make the two namespaces a type the set can hold, `EntryKey::Msg(MessageRef) | EntryKey::Draft(String)`, and to have each batch filter the set to its own half rather than assume homogeneity ([src/tui/app/types.rs](../src/tui/app/types.rs), #0052).
When a list holds two kinds of row, any set keyed on one of them is a dead end for the other, and the compiler cannot see it because `filter_map` on the absent field is perfectly well typed.

## The rule a command enforces is not always in the function named after checking

`mp send` calls `validate_draft`, which checks recipients, subject and address syntax, and says nothing about approval.
The approved-status requirement lives in `send::build_draft_message`, which refuses anything whose `status:` is not `approved` before the outbox row is written.
Porting the TUI's send by mirroring the validator alone would therefore have shipped a `s` key that sends unapproved drafts, which is the one thing the draft/approved split exists to prevent.
Mirroring a CLI path means calling the same functions in the same order, not reproducing the checks that look like checks ([src/send.rs](../src/send.rs), #0052).

## An editor window over a copy nothing reads back is a false affordance

The file build's TUI opened a received message, and a server-search hit, in `$EDITOR` by handing it the `.md` the ingest had written.
After #0037 there is no such file, and the tempting port is to materialise the message into a temp file and open that: the window looks the same, the keystroke works again, and no status line says "cannot".
It is worse than a decline, because the user's edits are accepted, saved, and dropped on the floor, which is the same family as a send that reports success on a failed submission.
The rule the branch settled on: port a flow only where the artifact behind it is real, decline permanently and say why where it is not, and let a materialised copy carry a flow only when the flow is inspection rather than composition (the calendar's Open event source hands `$EDITOR` a copy of the invite's `.ics`, which is worth reading and was never meant to be written back) ([src/tui/actions.rs](../src/tui/actions.rs), #0052).

## Overriding `$TMPDIR` per test wipes the temp dirs of every parallel test

The store-backed file tests wrote into `/tmp/mailypoppins-<row id>`, which is the exact path a real `mp open` of that row uses, so a test run and a live session collided on the same directory.
The obvious isolation, pointing `$TMPDIR` at the fixture's own tempdir and removing it on drop, is worse: `$TMPDIR` is process-wide, so every `tempfile::tempdir()` on another test thread lands inside the fixture's tree and disappears mid-test when the fixture drops.
The isolation has to outlive any single test: one directory per process, created once and never removed while the process runs, with the per-fixture part only setting and restoring the variable ([src/tui/actions.rs](../src/tui/actions.rs), #0052 review).

## A per-open integrity check turns every read into a walk of the whole file

`Store::open` ran `PRAGMA integrity_check` on every open, which is a full walk of every page: 240 ms on a 44 MB store.
The TUI opens a store per call rather than parking one, because `rusqlite::Connection` is not `Sync`, so the check ran once per `j`/`k` (the preview-body memo misses on every cursor move) and ten times before the first paint.
The startup and keypress latency the owner reported was one line of validation multiplied by an access pattern nothing had measured against it.
Deleting the check was not available either: the store has no migrator, and drop-and-rebuild-on-corruption is what stands in for one.
The shape that keeps both is amortisation, a process-wide registry keyed by canonical store path, so the first open of each file still validates in full and still triggers the rebuild while later opens skip it ([src/store/mod.rs](../src/store/mod.rs)).
The general form: a validation whose cost is proportional to the whole artifact must be tied to the artifact's lifetime, never to the handle's.

## A comment explaining why nothing needs invalidating is a landmine once the read path moves

`BgResult::Fetch` and `BgResult::Sync` set a status line and stopped, under a comment reading "Ingest writes no `.md`, so a sync never changes what the list is reading and there is nothing to invalidate".
That was true for exactly one ticket, the #0037 interim state where ingest wrote to the store and the list still read files.
#0038 moved the list onto the store and nobody went back for the comment, so refresh stopped refreshing: new mail, applied read flags and rows the server had dropped were all invisible until a mailbox switch or a restart.
The comment is what made it survive review, because it reads as a considered decision rather than as a gap.
A comment that justifies an absence should name the condition it depends on, so that grepping for the condition finds the comment when the condition changes ([src/tui/bg.rs](../src/tui/bg.rs), #0038 follow-up).

## A delete and the insert that justifies it must not be split across iterations of a loop

The prune deleted the rows a mailbox's fetch proved gone, and its safety argument was that a message archived in another client is re-ingested at its destination by the same sync.
That argument holds for the sync, not for one iteration of it: targets are walked in order (inbox, archive, sent), so a prune applied inside the loop deleted the inbox row a full mailbox fetch before the archive pass inserted the replacement.
In that window the store held no row for the message at all, its body blob dropped to refcount zero and was unlinked from disk, and an archive fetch that failed (the loop logs and continues to the next target) left the message locally lost until a later sync.
The fix is to collect the per-target diffs during the loop and apply every delete in a second pass after it, so the ordering the comment claims is the ordering the code has ([src/imap_client/store_sync.rs](../src/imap_client/store_sync.rs), #0038 follow-up).
When a destructive step is justified by a compensating step elsewhere, the two belong in the same phase, and the test that pins it has to assert the state *between* them (never zero rows, never a released blob) rather than only the endpoints.

## Registering a check before its verdict makes a failure look like a pass

`open_validated` noted the file as integrity-checked and then compared the result, so a store that failed the walk was recorded as checked for the rest of the process.
Only `Store::open`'s own `forget_integrity_check` on the rebuild path hid it, which makes the correctness of a cache entry depend on what the caller does with the error.
A memo of "this was verified" is written on the success branch, never before the branch ([src/store/mod.rs](../src/store/mod.rs)).

## A memo keyed on one row kind blanks the other kind instead of failing

The preview body was memoised under a key built with `self.selected_email()?.msg?`, and a draft row has no `msg`.
The `?` turned "this key cannot name a draft" into `None`, `None` filled the memo with an empty string, and the Body pane rendered blank for every draft with no error, no log line and no failing test.
Nothing in the code said drafts were unsupported; the capability gap was expressed only as a silently absorbed `Option`.
The fix was to widen the key to the enum that already names both kinds of row (`EntryKey::Msg` / `EntryKey::Draft`) so each arm has to be written out and a new kind of row breaks the match instead of the pane ([src/tui/app/mod.rs](../src/tui/app/mod.rs)).
Where a key is derived from a value that has more than one shape, prefer the enum over a chain of `?`: a `None` that means "not applicable" and a `None` that means "nothing selected" must not be the same value.

## Under server-as-truth, a local artifact that has been submitted has to be retired, not annotated

Sending a draft rewrote its `status:` to `sent` in place, which was correct while `sent/` was a directory of `.md` files and the draft was the only local record.
After #0037 the sent copy is the server's, APPENDed by the durable outbox and read back into the store, so the annotated draft became a second and staler copy of a message that had already left, sitting in the Drafts list and in `mp list` with nothing left to do to it.
An in-place status flag is the file build's way of saying "this moved on"; once something else owns the moved-on copy, the flag has to become a deletion.
The exception is the partial send: it keeps the marked file because the file is the only thing that still names the recipients who did not get it ([src/draft.rs](../src/draft.rs), `settle_sent_draft`).

## Deleting the local copy is only safe once the replacement copy exists

`settle_sent_draft` retired a fully sent draft on the argument that the server's copy replaces it: the durable outbox APPENDs the message to Sent and ingest reads it back into the store.
The argument holds only when there is an outbox row, and `send_durably` deliberately sends without one when the store cannot be opened, reporting `state: None` rather than refusing the send.
That branch is rare enough to be invisible in testing and is exactly the one where the deletion is unrecoverable: no APPEND, no ingest, and the recipients hold the only copy of the message.
A retirement now requires the report to be both complete and durable, which is a precondition on the *replacement* rather than on the send ([src/draft.rs](../src/draft.rs), `settle_sent_draft`).
When a fallback is written that trades a guarantee for availability, every later step that assumed the guarantee has to name it in its own condition; the fallback's own comment will not find them.

## A full rebuild is a deletion, so it needs the same precondition as one

`mp contacts rebuild` regenerates a derived index, which reads like a refresh and is not one: it replaces a corpus the send/sync hooks accumulate incrementally, and the only thing it can offer in exchange is what its source currently holds.
When the source was the deleted `.md` tree the exchange was a populated cache for nothing at all, and three call sites persisted it without looking (#0053).
The source being right again does not retire the guard, because the sources are not equivalent: an account whose store carries no rows yet still has months of hook observations, and a rebuild would still trade them for zero.
An empty result from a full rebuild is now read as a failure to read rather than as an empty world, and refused ([src/contacts/cache.rs](../src/contacts/cache.rs), `save_rebuilt_cache`).

## A detection window and a download window that are not the same window never converge

The Graph sync decided what was new by enumerating the whole folder and then downloaded it by asking the folder for its `$top` most recent messages.
For a message that is new to the store but old on the server, one message moved into Archive in Outlook web, the two windows never intersected: it was reported new on every sync and downloaded on none, and the `skipped.min(20)` over-fetch in the code was an admission that the numbers were not expected to line up.
No error surfaced, because both halves were doing exactly what they were written to do.
The fix is to make the download name what detection found rather than approximate it: fetch each detected id by id, twenty per Graph `/$batch` call ([src/graph.rs](../src/graph.rs), `fetch_messages_by_ids`).
When a diff is computed over one set and applied to another, the sync's progress depends on an overlap nothing in the code enforces; make the second set the output of the first.

## Reusing a client across a poll loop means owning its token's expiry

Rebuilding the Graph client on every watcher pass cost a keyring read and a fresh connection pool per minute, but it also hid a dependency: it refreshed the OAuth2 access token as a side effect.
Keeping one client for the life of the watcher removes the cost and the refresh together, and an access token expires after about an hour, so the loop would have started 401-ing mid-session with nothing in the code saying why.
`GraphClient::refresh_token` now updates the token in place and the watcher calls it once per pass, which keeps the pool and re-reads the cached token only ([src/graph.rs](../src/graph.rs), [src/tui/helpers.rs](../src/tui/helpers.rs)).
Before hoisting a construction out of a loop, list what the constructor did besides construct.

## A prune deletes by identity, so a row the server never listed under that identity is always "vanished"

The Graph prune computes `store − server` over `internetMessageId` and deletes the difference, which is correct for every row the server put there.
A Graph send does not produce one: `sendMail` takes JSON with no `Message-ID`, Exchange stamps its own, and the outbox has already filed the local copy under a uid derived from *our* id, so that row is in the difference on every pass from the moment it is written (#0065).
Deleting it releases the raw MIME, which on a Graph account is the only copy that will ever exist.
The IMAP path was immune by accident rather than by design: its prune was clamped to the fetch window's numeric range, and a `graph_uid` hash sits far above any real UID. Since #0072 the clamp is deliberate, `UIDNEXT - 1`, and IMAP runs the same age guard.
Two shapes of row are exposed this way, the locally synthesised one and the one whose server copy has not been filed yet, and both are recent; the guard is therefore an age window on the row rather than an exemption for the `sent` role, which would have made the duplicate permanent ([src/ingest.rs](../src/ingest.rs), `prunable_uids`).
When a diff drives deletions, ask which rows were written by something other than that diff's own source.

## A capped download and an uncapped diff are safe only one target at a time

A quick sync caps the download at `limit` and computes the vanished set over the whole folder, which reads as a conservative pairing: fetch less, delete only what is provably gone.
It is not, because the argument that lets an inbox row go is that *another* target ingested the copy the message moved to, and the target that was capped is the one holding that copy (#0065).
Gating each target's prune on its own coverage would have left exactly the reported case unfixed, since the inbox pass truncates nothing when a hundred messages are moved to Archive at once.
The gate is therefore the whole pass: every target enumerated in full and downloaded in full, or nothing is pruned this run ([src/graph.rs](../src/graph.rs), `pass_may_prune`).
When two halves of an operation have different scopes, the safety condition belongs to whichever half the *other* one depends on.

## A tolerant parse is a prune-safety property when the strict one fails a whole batch

`GraphBatchEntry.headers` was `HashMap<String, String>`, which is the honest type for what an HTTP header is and the wrong one for what a `/$batch` response is: the twenty sub-responses are parsed as one document, so a single header value serialised as a number or an array fails the deserialization of all of them.
That is not one lost message but zero downloads for that folder on every pass, and, since the prune now waits for a pass that downloaded everything it found (#0065), a prune suspended for as long as it lasts.
The value type is `serde_json::Value` and the seconds are extracted where they are read (#0065 follow-up, [src/graph.rs](../src/graph.rs)).
A field the code never reads should not be able to fail the parse of the fields it does; when a parse covers a batch, its strictness is measured in units of the whole batch.

## "Did it fetch everything" is not the same question as "did the limit truncate it"

The Graph prune gate derived its coverage flag from `found > new.len()`, the cap, and stopped there.
Everything else that makes a download short was invisible to it: a failed sub-response, a batch that spent its failure budget, a body that did not parse, an ingest that errored and only warned.
A throttled-out Archive pass therefore returned an empty vector, reported a complete download, and opened the gate on inbox rows whose archive copies had never landed, which is precisely the loss the gate was added to prevent.
The flag is now folded from the counts at each step, ids asked for against messages returned against rows written ([src/graph.rs](../src/graph.rs), `fetch_new_messages`).
A safety flag derived from one cause of a condition asserts the condition; derive it from the effect instead.

## A query error and an empty result are different answers, and `unwrap_or` merges them

`prunable_uids` read each candidate row's date with `.unwrap_or(0)`, and `0` is "very old", so a locked database or a malformed row made the row maximally prunable.
The intended meaning of the default was "no such row, so the delete is a no-op", which is true only for `QueryReturnedNoRows`; every other error arrived at the same place and failed open, on the one code path in the crate whose failure mode is deletion.
The arms are now separate, and a lookup that errors holds the row back (#0065 follow-up, [src/ingest.rs](../src/ingest.rs)).
`unwrap_or` on a query is a decision about every error the query can produce; when the caller deletes things, write the arms out.

## Four copies of one orchestration do not drift evenly; they drift toward the weakest error handling

`mp send`, `mp send-approved` and the two TUI send keys each carried their own copy of build, submit, settle, bump, reindex (#0058).
Diffing them before merging turned up three divergences that no test and no reader had noticed: the TUI's send-approved never refreshed the drafts index, the CLI's Graph send-approved silently dropped an attachment it could not read (`if let Ok(content)` where the other three propagated), and a Graph transport error in that same loop aborted the whole batch where the SMTP loop counted one failure and carried on.
None of the three is a copy-paste slip; each is a place where one copy was edited and the others were not, and the weaker handling is the one that survives because it never fails a test.
Unification is therefore not a mechanical merge: every divergence found is a decision to take deliberately and to write down, because whichever copy you started from silently becomes the contract for all four call sites.
When collapsing duplicated orchestrations, diff them first and treat the differences as the findings, not as noise.

## A capability guard that asks "did the config load?" turns a load failure into a silent fallback

`App::is_graph()` is `graph_config.is_some() && auth_method == Graph`, and both TUI send keys chose their transport with it (#0058).
`AccountState::new` loads the Graph config with `GraphConfig::load(..).ok()`, so an account that *is* a Graph account but whose config failed to load answers `false`, drops into the SMTP branch and sends the mail over whatever SMTP config the account happens to carry, from an identity Graph would have stamped differently.
The `Graph not configured` status written for exactly that case was unreachable, because the only way to reach it was for the account to be Graph with no Graph config, which is the condition the guard had just answered `false` to.
The guard now keys off `auth_method` alone and the missing config is the error (`tui::helpers::resolve_send_transport`).
A guard over "what is this thing" must not be conjoined with "did its optional setup succeed": the second question belongs to the branch, where its failure has somewhere to be reported.

## A shared status line is a last-writer-wins register, so it reports the slowest path and never the fastest failure

Three accounts sync concurrently at TUI startup and every one of them reports its outcome by writing `app.status_message` (#0068, #0071).
`perso` failed at IMAP login after 54 ms; `tum` and `assistant` finished fifteen seconds later and wrote `Fetch complete` over it.
The register does not lose a random writer, it always loses the fast one, and a failure that dies at authentication is always the fast one, so the arrangement systematically reports success and hides refused logins: seven weeks and roughly 2900 of them, in this case.
Adding a log line makes the failure recoverable after the fact; it does not make it visible.
The fix is that a per-entity outcome must be stored on the entity, not in a register shared with its peers, and rendered from there every frame ([src/sync_health.rs](../src/sync_health.rs), `AccountState::sync_health`).
Whenever N concurrent workers report into one slot, assume the slot shows the slowest one, and ask what the fastest one was trying to say.

## "It is only a cache" is a claim about a file, and one table in it can be the exception

The per-account store is dropped and rebuilt whenever its schema version moves or its `integrity_check` fails, which is safe because the server holds every message back (#0066).
The `outbox` sat in the same file and was deleted with it, although it is the record of what has already been submitted to a mail server and no sync can reconstruct it: a message accepted by SMTP but not yet copied to Sent disappeared, silently, and the v4 bump ran that path for every account at once.
Two things generalise.
A durability claim belongs to a table, not to the file the table happens to live in, so a disposable file needs its exception list written down next to the code that disposes of it ([src/store/rebuild.rs](../src/store/rebuild.rs)).
And any content-addressed store whose refcounts live in the disposable file leaks its whole tree on every rebuild, because the files outlive the counts; the drop has to sweep them or take the directory with it.

## A second doc-comment paragraph on a clap field reformats the whole subcommand's `--help`

Adding a rationale paragraph under the `///` line of a `Commands::Sync` field (#0071 review) flipped `mp sync --help` from clap's short help to its long help: every option grew a hanging description block, `--help` started advertising `(see a summary with '-h')`, and `tests/cli_help_snapshot.rs` failed with a 27-line diff for a change that touched no user-visible behaviour.
clap treats the first doc-comment line as `about` and everything after the blank line as `long_about`, and the presence of a long help on any argument switches that command's `--help` to the long format.
Explanatory prose that is not meant for the user belongs in a plain `//` comment above the `#[arg(...)]`.

## A SQLite scan does not skip a damaged page, it ends at one

Salvaging an outbox out of a store that failed `integrity_check` looked like a per-row concern: read the rows, log the ones that will not come back, carry the rest (#0066).
It is not, because `rusqlite`'s `Rows::advance` calls `reset()` on a step error and `reset` takes the statement, so after the first `SQLITE_CORRUPT` every later `next()` returns `Ok(None)`.
The loop's error arm runs once and the iterator then reports a clean end of table: 204 of 400 rows disappeared while the code counted zero discards and logged one skipped row.
An `Err` inside an iteration is the end of that iteration unless the API says otherwise, and a loop that treats it as "skip one, continue" silently truncates.
Reading a damaged table means addressing rows individually (`SELECT ... WHERE rowid = ?`, one query each, each seeking from the btree root), and counting what was reached against `COUNT(*)` or `MAX(rowid)` so the gap can be named instead of assumed to be zero ([src/store/rebuild.rs](../src/store/rebuild.rs)).

## A query with no `ORDER BY` has no "after the last row it returned"

The recovery built on the lesson above listed the rowids first and then re-read the positions the listing never reached, taking those to be the ones above the highest rowid it had returned (#0066 review follow-up 2).
The listing omits `ORDER BY` deliberately, so that the planner may serve it from the smaller `outbox_state` index instead of the damaged table btree, and that index is ordered `(state, rowid)`: a listing that stops mid-index has skipped rowids scattered *below* its own maximum, and every one of them was silently written off.
It looked correct because the test table held one state, where index order and rowid order coincide.
When a read is deliberately left unordered, the set it did not reach is the complement of what it returned, not a suffix, and the code that goes back for the remainder has to be written from the complement (`1..=MAX(rowid)` minus a `HashSet` of what was listed).
An unordered result read as if it were ordered is a bug that only the favourable test data hides.

## `lettre`'s `is_response()` does not mean "the server responded with an error"

The send path classified an SMTP failure as clean or ambiguous with `!(err.is_response() || err.is_client() || err.is_tls())`, on the reading that a response error is the server saying no in words (#0063).
It is not: `Kind::Response` is a reply that could not be *parsed*, while a 5xx refusal is `Kind::Permanent` and a 4xx is `Kind::Transient`, so every hard rejection fell through to the ambiguous branch and parked a row that the server had explicitly and finally refused.
The predicates that mean what they say are `is_permanent()` and `is_transient()`, and there is no public predicate at all for the distinction that would matter most, a TCP connect failure (`Kind::Connection`, nothing was sent) against an i/o error on an established stream (`Kind::Network`, the 250 may have been lost): both are only reachable as "none of the above".
When an error type's predicates are named after its internal variants, read the variants before writing the boolean.

## A WAL transaction that reads before it writes must begin IMMEDIATE

The outbox admission gate reads the open rows to decide whether it may insert one, inside a transaction opened with `rusqlite`'s `unchecked_transaction()` (#0063 review).
That begins DEFERRED, which takes the read snapshot at the first `SELECT` and the write lock at the first `INSERT`, and under WAL a writer that commits in between invalidates the snapshot: the insert fails with `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` does not retry because the transaction has already read stale data and can only be rolled back.
So the loser of a two-process race did not see the winner's row and be refused, it failed on the write, and the send path's fallback treated that failure as "the store is unavailable" and sent the message with no outbox row and no gate at all.
`BEGIN IMMEDIATE` (`Transaction::new_unchecked(conn, TransactionBehavior::Immediate)`) takes the write lock up front, so the two enqueues serialise and the second one reads what the first committed.
The second half of the lesson is about the fallback: an error path that downgrades to a less safe mode has to enumerate the failures it was designed for, because "everything else" will eventually include the one failure that means another process is holding the very lock the downgrade bypasses.

## Two derivations of one key drift, and the one nobody reads wins

The sidebar queried the store with the leaf of a `PathBuf` it built from the config (`mailbox_dir`), while the sync path handed ingest the configured name itself (#0064).
For `inbox`, `archive` and `sent` the two agreed, so nothing looked wrong; for an `[[mailboxes.extra]]` mailbox the path builder *slugified* the server name, so rows ingested under `Team/Reports` were listed and counted under `team-reports` and the mailbox appeared permanently empty, its quick-move destination wrote rows under a key sync never uses, and `mp dump-mailbox --mailbox Team/Reports` returned nothing.
The bug had been invisible for as long as it existed because no account here configures an extra mailbox, and the integration test that would have caught it had been written by ingesting under the slug, i.e. against the reader rather than against the writer.
When two sides of a boundary each derive the same key, the derivation belongs to one of them and the other reads it; a fixture that feeds the reader its own convention proves nothing about the writer.

## A diff computed over the download window answers a question nobody asked

The IMAP prune took the store's UIDs, kept the ones between the lowest and highest UID the fetch had just read, and deleted whatever the server had not listed in that range (#0072).
The clamp was there to stop a capped sync from deleting the backlog under its window, and it did, but it also made the diff blind in exactly the direction removals arrive: a message archived elsewhere is *absent* from the listing, so it cannot pull the window's bottom down to itself, and archiving the oldest mail first, which is what everyone does, put every removal below the clamp forever.
The fetch already held the complete answer and was throwing it away: `UID SEARCH ALL` enumerates the whole mailbox, and only the *download* is capped by `limit`.
The fix is to diff against the enumeration and keep one clamp at the top, `UIDNEXT - 1`, which is the line between a server-issued UID and a placeholder this client wrote ([src/imap_client/fetch.rs](../src/imap_client/fetch.rs), `vanished_uids`).
When a safety clamp is derived from a partial input, check whether the complete input was available in the same function; a clamp that hides a whole class of answer is a wrong query, not a cautious one.

## Coverage means "did the arrivals land", not "did the folder fit"

Gating the IMAP prune on the same condition as Graph's, every message found was downloaded, would have suspended it permanently: a quick sync's window is the last 100 UIDs of a mailbox that can hold 8000, so every capped pass reports a backlog it never intended to fetch and the gate never opens (#0072).
Graph gets away with the strict form because it downloads by id, so its backlog drains one window per pass; IMAP's window is positional and the backlog under it is skipped by design.
What the prune actually depends on is narrower: an inbox row may go because another mailbox ingested the copy the message moved to, and a move issues a fresh UID at the *top* of the destination, so the flag to compute is whether every UID above the mailbox's arrival mark landed ([src/imap_client/fetch.rs](../src/imap_client/fetch.rs), `arrival_coverage`).
When two backends share a safety gate, share the predicate and let each compute its own inputs; the flag's name travels, its derivation does not.

## A watermark recomputed from the store each pass measures the wrong thing after the first one

The IMAP prune gate held a pass back when a UID above the mailbox's high-water mark had not been ingested, and derived that mark afresh from `max(known)` every pass (#0072 review).
It therefore protected exactly one pass: bulk-move 300 messages into a mailbox whose quick sync downloads the top 100, and pass 1 defers correctly, while pass 2 stands on a mark its *own* ingest has just raised to the top of the folder, finds the 200 copies it never fetched sitting below it, calls itself complete, and prunes the source rows of messages that have no local copy at all and that a positional window will never go back for.
A gate whose input is recomputed from the state the gate's own action changes is a one-shot gate.
The mark is now persisted per mailbox (`sync_cursors.arrival_mark`) and the next pass is held to the lower of carried and derived, which is the only form that survives its own success ([src/imap_client/fetch.rs](../src/imap_client/fetch.rs), `arrival_coverage`).
Two exits keep it from becoming a deadlock: a pass that reaches through the mark clears it, and so does one where the missing arrivals stopped being listed.

## Gmail archives by removing a label, so the copy is not an arrival and no gate can wait for it

Pruning a row because the message moved rests on the destination mailbox ingesting the copy, and on servers that implement a move as `COPY` + `EXPUNGE` (Exchange, Dovecot) that copy arrives with a fresh UID at the top of the destination folder, where the same pass fetches it.
Gmail moves nothing: archiving removes the `INBOX` label and the copy in `[Gmail]/All Mail` keeps the UID it was given when it first arrived, which is usually far below the bottom of a capped window (measured: uid 1 against a window bottom of 234).
The arrival gate cannot help, because by its own definition, correctly, that copy is not an arrival.
The behaviour is therefore correct but asymmetric: the inbox row is pruned on the next quick sync and the archived copy is re-filed only by a full sync of the archive mailbox.
Anything written for users about removals converging "on the next sync" has to say so ([website/src/pages/faq.astro](../website/src/pages/faq.astro)).

## A conservative default answer, once persisted, stops being a default

The same arrival mark that had to be persisted to survive its own success (the entry above) then had to learn not to be persisted at first contact.
The mark is derived from what the mailbox is known to have held, an empty store knows nothing, and `high_water` answers `0` for an empty set, so the first capped sync of a mailbox bigger than the download window wrote a mark of `0`: the line that says every message the server lists must be in the store before any pass counts as complete, which a positional window of 50 or 100 never reaches, and which the carrying rule (`carried.min(derived)`) could never let rise again.
Because the prune needs every mailbox complete before it applies anything, one such mailbox held the removals of the whole account, and the schema bump that shipped alongside rebuilt every store, so the state was universal rather than rare (#0072 sweep review).
"Assume the worst when you know nothing" is the right answer for one pass and the wrong thing to write down, because a record of ignorance is indistinguishable from a record of a real obligation.
The fix is to make the two distinguishable rather than to soften the mark: no cursor row is first contact, where everything the server lists is backlog and nothing is owed, while a cursor row is history even when every local row is gone, and its recorded top (`sync_cursors.last_uid`) is what a mailbox emptied elsewhere and then bulk-moved into is measured against.
Before persisting a value a later run will be held to, ask what it means when it was computed from no information; if that reads the same as a genuine obligation, it is the absence that has to be representable ([src/imap_client/fetch.rs](../src/imap_client/fetch.rs), `arrival_mark`).
## A lifecycle with no terminal state leaves its dead ends on disk

A draft can be created, edited, approved and sent, and nothing deletes one ([#0073](tickets/0073-delete-draft.md)).
`mp delete` takes a received selector, `mp://<account>/<mailbox>/<message-id>`, and a draft has neither a mailbox nor a message, so the grammar cannot express the request at all; the TUI meanwhile binds `d` once for every view, and `Action::Delete` hands `selected_email_ref()` to a store mutation that finds nothing to prepare, which reaches the user as `Delete failed: nothing to delete` ([src/tui/actions.rs](../src/tui/actions.rs), `Action::Delete` and `delete_msgs`).
The gap stayed invisible while send removed the file on its own, and appeared only on a machine upgraded from a version that did not, where nine `status: sent` drafts had no supported way out.
The index side needs nothing built: `mp list` re-scans the drafts directory and drops the rows whose file is gone, so removing the file is the entire missing operation.
A key bound once across every view is a promise made in every view, and the action behind it has to resolve per view or the binding lies in all but the one it was written for.

## A flag column with one flag in it invites code that writes the string, not the flag

`messages.flags` held `\Seen` or the empty string, and every writer treated it as a boolean spelled out in SQL: `set_read` wrote the literal `"\\Seen"`, `apply_seen_flag` overwrote the column with it, ingest derived it from one bool.
That is exactly right until a second flag exists, at which point every one of those writes silently erases what it does not know about, and the erasure is invisible because the next sync restates the server's answer and hides the local loss (#TKT-0051).
Adding `\Answered` and `$Forwarded` was therefore not a matter of writing two more tokens but of making every writer read-modify-write through one parsed type (`types::MessageFlags`), and of splitting the sync entry point in two: IMAP states the whole flag set and may replace it, Graph knows only `isRead` and may merge one bit.
A column that encodes a set has to be written as a set from the first flag onwards, or the second one arrives as a bug in code nobody thought was about flags.

## The flag column is a cache, which is what makes a second flag free

The axis needed no schema bump, and the reason generalises: pass 1 of every IMAP sync fetches `FLAGS` for the whole download window, so a store that has never heard of `\Answered` fills the column in on the next pass with no migration and no re-download.
A bump would have rebuilt every store and cost every user their backlog for a column that was already there and already refilled from the server on a timer.
Before bumping `SCHEMA_VERSION`, check whether the new state is something the server restates anyway; if it is, the additive write is the whole migration.

## Gmail accepts the `$Forwarded` keyword and hands it back as a custom flag

Measured live on a Gmail account (#TKT-0051): `UID STORE +FLAGS ($Forwarded)` is accepted, survives, and comes back from `FETCH FLAGS` as `async_imap::types::Flag::Custom("$Forwarded")` rather than as any system flag.
`\Answered` round-trips as `Flag::Answered`, which is what a reply sent from any other client sets, so the answered state syncs both directions without anything mailypoppins-specific on the wire.
Read keyword matching case-insensitively and accept `\Forwarded` as well: the keyword is registered as `$Forwarded` (RFC 5788) but not every server spells it the same way, and only one spelling should ever be written.

## A capability deleted by a rewrite leaves no failing test behind it

Every received message was a `.md` file until the store cutover ([#0037](tickets/0037-sqlite-store-engine-skeleton.md)), and `$EDITOR` on a list row opened that file.
The cutover deleted the files, the `EditCurrent` arm was rewritten to decline on a received row, and a comment was added saying the decline was permanent "because nothing is coming that would make it work" ([src/tui/actions.rs](../src/tui/actions.rs), `Action::EditCurrent`).
Nothing was broken by that in the sense a test can see: the store was truth, the preview pane still rendered the message, and the decline was honest about the file.
What was lost was the affordance, and it stayed lost for months because a rewrite that removes a mechanism removes the thing that would have complained about it ([#0075](tickets/0075-open-received-mail-in-editor.md)).
The rendition was ten lines away the whole time: the same flow already materialised the browser `.html` and the invite `.ics` out of the store on demand, so the message itself was the one artifact with no rendition.
When a rewrite makes a mechanism impossible, write down what the mechanism was for, not that it is gone; the second is a fact about the code and the first is the thing that still has to be answered.

## A 0444 rendition has to be removed before it is rewritten

The read-only message view is written mode 0444 so `$EDITOR` opens the buffer read-only, and it lands in the per-row materialisation directory keyed by row id, so opening the same message twice targets the same path.
The second write then fails on the mode the first one set, and `fs::write` truncating an existing file gives no way around it.
Remove the file first and treat `NotFound` as success ([src/tui/actions.rs](../src/tui/actions.rs), `write_readonly`); the rendition is rebuilt from the store on every open, so there is nothing to preserve, and a stale copy surviving a rebuild would be worse than the failure.

## A value written into a double-quoted YAML scalar has two dangerous characters, not one

`draft::source_message_id` dropped a `Message-ID` containing `"` and wrote everything else into `in_reply_to: "<...>"` (#TKT-0051 review).
A backslash is an escape inside a double-quoted YAML scalar, so `<a\b@x>` is read back mangled as `<a\u{8}@x>` and `<a\qb@x>` is not a valid escape at all: it fails the scan, which fails the *whole draft's* parse, and the reply then disappears from the drafts index with only a log line behind it (the silent-skip mode #0064 named).
Wherever a header value is interpolated into a quoted scalar rather than serialised, both `"` and `\` have to be handled, and the same holds one layer out for an IMAP quoted string, which escapes exactly those two characters (RFC 3501 section 4.3).
Dropping the value beats escaping it when the value only exists to be looked up again, and a `Message-ID` carrying either character is not one a server issued.

## Parallel network fetch is safe only if it never touches the store

Sync got a per-mailbox parallel fetch ([#0005](tickets/0005-parallel-imap-fetch-per-mailbox.md)), and the temptation is to parallelise the whole loop, fetch and ingest together.
That loop carries the #0072 prune gate, the arrival marks and the deferred second prune pass, all of which depend on ingest happening serially in target order: inbox before archive before sent, so a message archived elsewhere has its destination row before its source row is pruned.
The change that keeps every one of those invariants is to split the loop into three phases and make only the middle one concurrent: read each mailbox's skip list from the store serially, fetch every mailbox in parallel with no store access at all, then ingest serially in target order exactly as before.
The concurrency runs on `futures::stream::buffered`, not `buffer_unordered`, because `buffered` yields its results in input order however the fetches finish, so the serial ingest still sees its mailboxes in target order and the SQLite single-writer discipline is never in question.
The load-bearing property is that swap, and it is one identifier, so it is pinned by a test that stalls the first future longest and asserts the output still comes back in order.

## A read-side jump across mailboxes needs a one-shot target the async load consumes

The conversation overlay ([#0008](tickets/0008-threading-conversation-view.md)) opens a message that may live in another mailbox, but a mailbox switch does not load synchronously: `switch_mailbox` on a cache miss shows an empty list and queues a background walk that lands later as `BgResult::MailboxLoaded`.
So "select this row" cannot be a single post-switch line; the target is parked on `App::pending_select` and consumed in two places, the cache-hit tail of `switch_mailbox` and the `MailboxLoaded` handler after its own cursor restore.
`consume_pending_select` clears the target only when it finds the row, so the parked value survives the async gap yet never lingers to hijack a later unrelated switch, because the row it names is always in its own mailbox's listing.

## row_columns is shared, so a new column shifts every hand-indexed extra column after it

Adding `thread_id` to `store::read::row_columns` shifted the trailing invite `EXISTS` predicate from index 12 to 13, and `row_from_sql` was updated to match.
The trap is `list_invites`, which appends one more column (`b.hash`) to the same shared `row_columns` string and reads it by a hard-coded index that was one past the end: that index moves too, and nothing but a runtime `Invalid column type` panic flags it, since the query still compiles.
Any column added to `row_columns` has to be chased into every query that concatenates extra selected columns onto it and reads them positionally.

## A wrong-store lookup can print a right-looking error, so parse the selector's account before opening any store

`mp delete mp://tum/drafts/<id>` under a non-tum default failed with `no match for <id> in the drafts index of tum/drafts`, while `mp delete -A tum <same>` succeeded ([#0073](tickets/0073-delete-draft.md) follow-up).
The selector parser already lets an `mp://<account>/…` segment override `-A`/the default, but every command in `main.rs` opened its store from the pre-chosen `account_config` *before* parsing, so a cross-account selector was resolved against the wrong account's index; the miss then formatted the scope from the query's own account (`tum/drafts`), naming the account the caller asked for while having searched a different one.
The error looked correct because it echoed the selector, which is exactly what made the bug hard to see: the fix is to resolve the account from the selector first (`account_for_selector`) and only then open the store and, for received mail, load the server credentials.
Where a command binds its transport before the selector is parsed (`mp send`, `mp invite`), a cross-account selector cannot be honoured cheaply, so it fails loudly with "selector names account X but this command is bound to Y" rather than acting on the wrong account.

## A skipped draft is invisible, so the scan has to hand the skip back as data, not a log line

The drafts index catches an unparseable `.md` file and keeps refreshing the rest (one bad file must not hide the other twenty), but the original catch was `Err(e) => log::warn!(...)`, which dropped the file with no row and no signpost: the draft vanished from the TUI Drafts list and `mp list` while sitting on disk ([#0080](tickets/0080-surface-parse-skipped-drafts.md), the "my draft disappeared" report).
The fix is the same shape the id-collision case already used: `refresh_reporting` returns the skipped files (path plus a one-line parse error) beside the collisions, so a caller can put the broken file back in front of the user instead of the log swallowing it.
`mp list` prints them after its listing and the TUI shows each as an error row.
The row is carried on a typed `EmailEntry::skip` field rather than the `(msg: None, draft_id: None)` pair, because that pair already means "server-search hit" everywhere in the action layer; overloading it would have let a skip row reach the wrong handler, so the third kind of row is its own field.

## A cursor-highlighted row hides its own foreground colour in a golden frame

The golden-frame style legend records `fg:error` now, and a parse-skipped draft row is drawn in it, but the drafts frame parks the cursor on that row so the preview can show the parse error, and the table's `row_highlight_style` (bg:surface + fg:selection) wins on the cursor row.
So the error colour does not appear in that row's style run at all; it is proven instead by the calendar frame, where a cancelled event's `cancelled` label is the same `fg:error` off the cursor, and by a `list_row_style` unit test.
When a frame needs to prove a row's own foreground, the cursor has to sit somewhere else, because the highlight is a full restyle, not an overlay.

## A backoff gate keyed on wall-clock `updated` makes a test that passes a small synthetic `now` a silent no-op

The durable queues stamp every row `updated = unix_now()` and gate a retry on `updated + backoff_secs(attempts) > now` (`src/outbox.rs`, `src/pending_ops.rs`).
A fresh row has `attempts = 0`, so the backoff is `0` and the row is eligible the instant `now >= updated`, which in production it always is because the drain passes `unix_now()`.
A test that enqueues a row and then drains with a tidy synthetic clock (`now = 10_000`) trips over the fact that `updated` is a real ~1.7e9 timestamp: `updated + 0 > 10_000` is true, so the row is treated as still backing off and the drain silently processes nothing, `completed == 0` with no error.
The gate is correct; the test's clock was below the row's real one.
Drive these drains from `unix_now()`-relative offsets (`unix_now() + 10` for immediate eligibility, `base + N * 10_000_000` to step past successive backoff windows), because the state transitions that stamp `updated` (`bump_attempt`, `fail_and_roll_back`) also use the real clock, not the `now` the drain was handed.
When a scheduler mixes an injected clock (the drain's `now`) with a wall-clock one (the row's `updated`), a test has to speak the wall-clock one or its rows look permanently not-yet-due.

## A succeeded server op that crashes before its queue row retires must converge on replay, or it surfaces as a false failure

The durable mutation queue (`src/pending_ops.rs`) commits the local write and the op it owes in one transaction, then a background drain runs the server op and retires the row.
The dangerous window is server-op-succeeded-then-crash-before-retire: on restart the row is still `queued`, the drain replays the op, and the message is already gone from the source folder.
Some backends reported that not-found as a plain `Err` (`imap_client::move_email_on_server` / `delete_email_on_server`, `graph::mark_read_graph`) while others returned `Ok` (the IMAP flag ops, `graph::move`/`delete`), so a replayed IMAP move retried to `MAX_ATTEMPTS`, parked a *succeeded* op as `failed`, and `fail_and_roll_back` moved the local row home: local diverged from the server until the next full sync, and a completed move was shown as failed.
The "idempotent by Message-ID, a no-op on both backends" claim in the module doc was simply false for those three paths.
The fix is a typed not-found signal, not string matching: the backends return `ops::NotFoundOnServer` and the drain treats it as a converged replay (retire the row, no rollback), while every other error stays a genuine failure that rolls back once the budget is spent.
Keep the typed error's `Display` byte-identical to the old `anyhow!` text so direct CLI/TUI callers still show the user the same "not found" message; the split is drain-converges vs caller-errors, decided by `NotFoundOnServer::is_in`, not by changing what the backend prints.
Test honesty matters here: a fake executor scripted to return `Ok` on replay validates the harness, not the backends, and hides exactly this blocker, so the crash-replay test must script the real not-found and assert convergence, paired with a genuine-error test that still rolls back.

## The same not-found is a converged replay for the background drain but a real error for the synchronous CLI

Wiring the consumers onto the queue (#0039 piece 4) surfaced that the not-found policy is caller-dependent, not a property of the op.
The background drain (`pending_ops::drain`, used by the TUI and the sync-tick `resume_account`) converges a `NotFoundOnServer`, because it cannot tell a genuine miss from a crash-replay of an op whose server half already landed, and converging is the only choice that does not surface a succeeded op as failed.
The CLI wants the opposite: `mp delete <id>` for a message the server no longer holds must print the not-found error, and a regression pins that string byte-identical.
The resolution is that the CLI does not use the converging drain at all.
`pending_ops::run_and_settle` enqueues, runs the single owed op, and returns its raw result, retiring on success and rolling the local half back on any failure including not-found, because a synchronous caller runs the op once in the process that enqueued it and so is never a crash-replay.
So the queue is one seam with two settle policies (`drain` converges, `run_and_settle` reports), and the discriminator is which entry point the caller picked, not what the backend returned.
Keep `run_and_settle` testable offline by splitting the settle decision (`settle(store, blobs, row, outcome)`) from the one `await` that produces the outcome; the decision is what a not-found regression needs to exercise, and it needs no live backend.

## Not every queued op has a rollback, and the post-send flag is the one that must not

The durable queue's failure policy is "roll the local half back to the server's truth and park the row as `failed`", which is right for a move or a read toggle: the user asserted a wish, the server refused it, so the wish is withdrawn.
The post-send `\Answered` / `$Forwarded` bit (#0076) is a different kind of statement.
It does not assert a wish; it records a fact, that the reply left the building, and no `UID STORE` refusal makes that fact untrue.
Rolling it back would replace a true local statement that the next sync can correct with a false one, on the tail of a delivered send, which is the last place an automatic undo belongs.
So its rollback is `Rollback::None` and its convergence is the sync, which restates every flag the server holds over the whole window.
The generalisation: pick a queued op's rollback by asking what the local write *means*, not by copying the neighbouring kind's.
A wish-shaped write rolls back; a fact-shaped write never does.

## Bookkeeping that rides the send path must be enqueued, never dialled

The same #0076 pass moved that flag write off the send path entirely, and the reason is worth keeping.
The old shape opened a connect-TLS-login-SELECT IMAP session per mailbox the source was filed in, inline, after a successful `send_draft`, so a message held in inbox, archive and sent cost three sequential logins on the tail of every reply, multiplied per draft by `mp send-approved`.
The invariant it had to preserve is narrow and absolute: bookkeeping may never fail, delay or re-send a delivered message.
Inline best-effort satisfies it only by swallowing every error, which also means the write is simply lost when it fails.
Enqueuing satisfies it better: one `COMMIT` on the send path, no network at all, no `outbox` row touched so the exactly-once submission marker (#0063) cannot be reached from here, and the failure is retried by the drain instead of dropped.
The offline pin for "a failed flag never re-sends" is to record a real submission, drive the flag op past its retry budget, and assert the outbox row keeps its terminal state and `sweep_pending_sends` finds nothing resubmittable.

## A fake backend only earns its keep if the failure it fakes is the failure the code has

Extracting the `SyncBackend` seam (#0059) put the sync loop under test, and the first three engine tests passed for the wrong reason.
They handed the ingest path bytes like `b"not a message"` to stand in for a message that downloads and will not write, and `mailparse` accepted every one of them: garbage with no colon, no headers, invalid UTF-8, even an empty slice all parse into an email with `(unknown)` senders.
The tests were asserting on a *successful* ingest of nonsense.
The one shape `parse_rfc822_to_fetched_email` actually rejects is a header block that opens with a continuation line (a leading space), which is what the fixture uses now.
The generalisation for any fake: assert that the fake's failure path is reached, not merely that the outcome you expected appeared, because a lenient parser turns "this must fail" into a test that pins nothing.

## A pooled IMAP session's real hazard is the response nobody read to the end

Making sessions persistent (#0041) looks like a lifetime problem and is actually a stream-framing problem.
IMAP is a single ordered byte stream with tagged responses, so a command whose response was abandoned half-read leaves bytes in the socket that the *next* borrower decodes as the answer to its own command: a `SEARCH` result read as a `FETCH`, a `BAD` attributed to the wrong request, or a hang.
`async-imap` makes this easy to hit because several commands return a `Stream` (`uid_fetch`, `uid_store`, `expunge`, `list`) and dropping it early is silent and legal.
So the pool's load-bearing rule is not the idle timeout or the `NOOP` probe, both of which only catch a *dead* connection; it is that any borrower whose operation returned `Err` poisons the session instead of returning it.
Poisoning a healthy connection costs one reconnect, which is exactly what the code did before the pool existed, so the conservative side of that trade is free.
The generalisation: when you start reusing a protocol connection, the new invariant is "the stream is at a known boundary", and every early return in every call site is a place it can be violated.

## Read a client library's command builder before designing around its API

The `CHANGEDSINCE` spike in #0041 was scoped as "the only unconfirmed detail" and took one `grep`: `async-imap`'s `uid_fetch` is `format!("UID FETCH {} {}", set, query)` with no validation of `query`, so an RFC 7162 fetch modifier is just the tail of the query string and needs no API support at all.
Two adjacent findings came from the same read: the response parser already decodes the `MODSEQ` item that CONDSTORE adds to every `FETCH` reply, so enabling it does not break the existing decode, and `select_condstore()` exists and fills `Mailbox::highest_modseq`.
The habit worth keeping is reading the vendored source in `~/.cargo/registry` before designing around a crate's surface, because the answer to "can I express X" is usually visible in one `format!` and is rarely in the docs.
The corollary is a limit on what that buys: `mock_stream` is private and `ImapSession` is monomorphic over our own TLS stream, so there is no seam to script a fake server through, and the wire form can only be pinned by testing the command builder, not the conversation.

## A resume token is not an observation, and a shared UPSERT must not treat it as one

`record_mailbox_cursor` wrote every column unconditionally from the caller's struct, which is right for `last_uid`, `uidnext` and `exists` (each pass observes them afresh) and wrong for `highest_modseq` and `deltalink`.
Those two are produced by one path and passed as `None` by every other, so the first full-window sync after a CONDSTORE sync erased the resume point, the next pass found NULL, fell back to the full window with no error, and the delta would have oscillated forever.
The tell is the type: an `Option` column whose `None` is produced by callers that *have no opinion* rather than callers that *observed nothing*, which means `None` cannot mean the same thing as it does for the columns beside it.
`COALESCE(excluded.x, x)` fixes it, and then a second, explicit path has to exist for the one case that must genuinely clear the token, here a UIDVALIDITY reset invalidating the modseq.
Caught in review before a real modseq was ever written; a bug of this shape is invisible once shipped, because its symptom is a fast path quietly not being taken.

## A delta that cannot name what it deleted must hand the deletion back, not guess

Microsoft Graph's `/messages/delta` reports a deletion as an entry carrying Graph's own message `id` and `@removed`, and nothing else.
The store keys a Graph row on `ingest::graph_uid(internetMessageId)`, which that entry does not carry and which the server will not sell back for a message it has just deleted, so the removal cannot be resolved onto a row at all (#0042).
The two tempting answers are both wrong: dropping removals on the floor makes deleted mail immortal locally, and persisting Graph's id per row to resolve them is a schema column and a store rebuild for every account bought for deletion *latency* rather than correctness.
What shipped instead is escalation: a pass whose delta reports any removal throws the change set away and enumerates the folder, so the prune keeps its existing `known − enumerated` source of truth and every coverage gate around it sees unchanged inputs.
The generalisation for any incremental protocol: check early whether its change events carry the identity *your* store is keyed on, because a delta that can add but cannot subtract is only half a sync, and the honest half is the one that falls back.

## What an incremental resume token asserts is a property of the *store*, not of the server

`HIGHESTMODSEQ` and a Graph `deltaLink` both look like server bookmarks, and reading them that way is how a delta path silently skips messages.
The load-bearing statement is about the local side: "at the moment this token was minted, the store held everything the mailbox listed".
Once that is the definition, three rules follow mechanically and stop being judgement calls: only a pass that covered the whole mailbox *and* wrote every message it found may mint one (#0041's `modseq_to_record`, #0042's `may_record_delta_token`), a token minted with a `$deltatoken=latest`-style shortcut must be taken *before* the listing it is stored alongside rather than after, or the window between the two is swallowed for good, and the ingest-failure bound has to reach the token as well as the prune gate (#0074), because a message the store will never accept would otherwise hold the resume point still for ever.
The same definition is what makes the discard rule trivial to write: any doubt at all, expiry, a page cap, a chain that ends with no resume point, a folder identity that changed under the token, drops it, because a delta that did not complete is indistinguishable from one that skipped a message.

## Every backend's incremental path needs a UIDVALIDITY equivalent, and Graph's is the folder id

A Graph delta token is bound to a folder *id*, while the config names a role (`archive`) and the client resolves it to a well-known name or a path.
Those are different identities: an `Archive` deleted and recreated in Outlook is the same name and a different folder, and a stored token that outlived that is a token for a mailbox that no longer exists.
Graph would answer it with a 404 or a 410, which the fallback catches, but depending on a server error for a local invariant is how the #0004 class of bug gets in, so #0042 reads the folder id (`$select=id`, one small GET per target) and stores its hash in the `uidvalidity` column, which is the analogous column on purpose.
Whenever a backend gains an incremental path, the question to ask before the token is designed is "what renumbering or re-creation makes this meaningless", and the answer belongs in a column the client checks itself.

## A contentless FTS5 index is a query surface waiting for a translator, not a search feature

`messages_fts` shipped with #0038 and was maintained correctly from day one: written in the same transaction as the `messages` row, deleted from every delete path (`delete_row`, `apply_delete`, the prune through `delete_by_uid`).
So #0043, "FTS5 full-text search", found scope items 1 and 3 already done and the whole of the remaining work sitting in the two things nobody writes down: how a user's typing becomes a MATCH expression, and how the answer is ranked.
Both are traps if skipped.
Handing user input straight to `MATCH` makes ordinary typing a syntax error (`c++`, `(draft)`, a trailing `AND`, one stray quote), and the fix is not escaping but wrapping: every term becomes a double-quoted FTS5 string literal, in which only `"` is special and is escaped by doubling, and the terms are joined by whitespace because FTS5's implicit operator is already `AND`.
Ranking is the same shape of omission: without an explicit `bm25()` in the `ORDER BY` the rows come back in rowid order, which reads as "sorted by nothing", and the weights are where the product decision lives (subject 10, sender 5, body 1: a word in the subject outranks the same word buried in a quoted reply chain).
The generalisation: an index is not a feature until something translates a question into it and orders the answer.

## Contentless FTS5 constrains the API more than the storage

`content=''` with `contentless_delete=1` buys the delete-by-rowid that external-content could not give us, and it takes three things away that a search feature usually assumes.
`snippet()` and `highlight()` fail outright, so the result rendering has to come from the joined `messages` row (its stored `snippet` column) rather than from the index.
`SELECT`ing an indexed column fails too, so a `MATCH` query is only ever a rowid producer, and the join is mandatory.
And there is nothing to rebuild the index *from*: the body text lives in a blob and the index keeps no copy, so `INSERT INTO t(t) VALUES('rebuild')` is not available and no repair path can exist.
That last one is only acceptable because the store is a cache with a drop-and-rebuild contract; what #0043 added instead of a repair is `store::search::index_drift`, two `NOT IN` counts that make the row/index invariant *checkable* in a test rather than merely asserted in a comment.
The rule of thumb: pick contentless when the content has another home and the file is disposable, and expect to pay for it at the API boundary, not in the writer.

## The literal words of an old ticket lose to a documented decision made after it

#0043 was written before #0038 and says "the `\` search path queries FTS5 instead of streaming files".
By the time it was implemented the `\` path no longer streamed files: it was an incremental, case-insensitive *substring* filter over the loaded mailbox, and `src/tui/app/types.rs` carried a written rationale for why it must not be served by FTS5 (token matching changes the result set for exactly the queries that mode exists for, a fragment inside a word, punctuation, a partial address, and none of it survives translation to a MATCH expression per keystroke).
Implementing the sentence would have regressed a deliberate decision to satisfy a ticket whose premise had expired.
The habit: when a ticket's scope item names the *mechanism* of a surface, re-read that surface before believing the item, and when the code disagrees with the ticket, the one with the dated reasoning next to it wins; the ticket close-out then records which item was superseded and by what, so the next reader does not re-litigate it.

## A random hex id written into YAML is a number once every thousand drafts

`drafts::new_id` minted 16 random hex characters and the index wrote them into the draft's frontmatter unquoted.
`8808e70039225152` is a valid YAML float in scientific notation, so the `Option<String>` field deserialised to `None` and the next refresh minted a *different* id, silently changing the draft's identity; a 16-digit id is an integer, which fails deserialisation and drops the draft from the index entirely.
About one id in a thousand has one of those shapes, so it surfaced as three unrelated-looking tests failing intermittently on unrelated commits, and the standing hypothesis for two years of it was a temp-dir/env-var race between parallel tests (#0077's own title).
The tell that it was not a race: every symptom was reproducible from a *single* file with no concurrency at all, once the id was chosen rather than sampled.
The rule: any identifier that will be written into a schemaless text format has to be constrained so it cannot be read back as another type, at the point it is minted rather than at every point it is written -- a leading letter costs two bits and closes the whole class ([src/store/drafts.rs](../src/store/drafts.rs), #0077).
And when a flake resists reproduction under load, look for a value-dependent bug before an interleaving one: a 1-in-1000 input is indistinguishable from a rare race until you ask which inputs fail.

## `std::env::set_var` in a test fixture is a data race, and a mutex over the writers does not fix it

The test harness pointed `MAILYPOPPINS_DATA_DIR`, `TMPDIR`, `HOME` and `MAILYPOPPINS_CONFIG_DIR` at a tempdir with `set_var` and restored them on drop, serialised by a crate-wide `data_dir_lock`.
The lock made the *writers* mutually exclusive and did nothing about the readers: every `tempfile::tempdir()` on another test thread calls `getenv("TMPDIR")` without it, and glibc's `setenv` can reallocate `environ` underneath that read.
This is why Rust 2024 made `set_var` `unsafe`; it is not a tidiness point.
It also cost the suite its parallelism, because every data-dir test queued behind one mutex.
The fix is to remove the shared state rather than guard it: `config::test_env` keeps the overrides in thread-locals, and since libtest runs each test on its own thread a fixture's paths are invisible to every other test -- no lock, no serialisation, no environment mutation anywhere in the binary ([src/config.rs](../src/config.rs), #0077).
The seam has to cover *every* reader inside the binary to help, which is why `parse::materialisation_root` moved too.

## Terminal graphics are painted over the cell grid, not into it

An inline image (#0010) is not cells: kitty, iTerm2 and sixel all paint pixels
the terminal owns, positioned by the cursor at the moment the escape sequence
is written, and ratatui's buffer knows nothing about them. Three consequences
shaped the implementation and will shape anything that touches it.

(1) **Clipping does not happen.** A `Paragraph` that overflows its pane is cut
at the border; an image that overflows is drawn over whatever is next to the
pane, border included. The preview therefore draws an image only when the whole
block it reserved is inside the pane (`images::fits_within`), and a
half-scrolled image simply does not appear. "Draw it and let the layout clip"
is not an option that exists.

(2) **A `TestBackend` golden cannot capture one honestly.** `ratatui-image`
smuggles the escape sequence through the buffer, so a headless golden of an
image pane would snapshot raw protocol bytes -- unreadable, and different per
protocol and per cell size. The fix is not to exclude image cells from the
snapshot but to make the headless path structurally imageless: the capability
query runs in `tui::run` alone, so no test process ever has a picker, every
`PreviewImage` carries `protocol: None`, and the golden captures the
placeholder contract instead. A golden that *could* contain an image cell is a
golden that will churn.

(3) **Halfblocks are a different feature, not a fallback.** `ratatui-image`
defaults to drawing images as coloured half-block characters when the terminal
answers nothing. That is real output in real cells -- it would change what
every non-graphics terminal shows and would land in the goldens. It was mapped
to "no graphics" deliberately; the ticket's fallback was a text placeholder.

The capability query has its own trap: it writes to stdout and reads the reply
off stdin, so it must run after the alternate screen is up and before the event
loop starts polling keys. A concurrent reader eats the reply and the terminal
looks capability-less.

## Pick the image formats the binary carries, or `image` picks all of them

`ratatui-image` exposes an `image-defaults` feature that turns on the `image`
crate's whole default format set: exr, avif (via `ravif`), y4m, tiff, plus
`rayon`. Adding it pulled ~60 crates for formats no email has ever carried.
Depending on `image` directly with `default-features = false` and
`features = ["png", "jpeg", "gif", "webp", "bmp"]` costs 16 crates instead --
cargo's feature unification then gives `ratatui-image` exactly those decoders.
Note that `cargo add` *merges* features into an existing dependency line rather
than replacing them, so removing `image-defaults` meant editing `Cargo.toml` by
hand.

## Memoising the preview body: the `Vec<Line>` was already owned

`wrap_and_style_body` was typed `fn(&'a str) -> Vec<Line<'a>>`, which read as if
the styled lines borrowed the body. They do not: every span is built from an
owned `String` (`to_string`, `format!`, `word_wrap`'s output, and
`parse_inline_markdown`'s `Vec<Span<'static>>`). The `'a` was cosmetic, so
memoising the product across frames (#0093) needed only the return type widened
to `Vec<Line<'static>>`, no ownership rework.

Two more traps for that cache. (1) Key it in O(1) or the compare is the cost you
were removing: comparing the whole body `String` every frame is itself O(body
length), so `PreviewBody` got a content `epoch` that bumps only on a real
change, and the cache keys on `(epoch, width, image set)`. The skip-draft and
primed paths refill the same text every frame, so the bump has to be gated on an
actual `(key, text)` change or the epoch churns and the cache never hits. (2)
Theme is not a cache key: it is set once at startup through a `OnceLock`
(`theme::init`) with no in-session switch, so the colors baked into the cached
lines can never go stale. If a runtime theme toggle is ever added, it must
invalidate this cache (and the invite/image memos, which also bake theme
colors).

The dirty-flag redraw (same ticket) has one non-obvious safety point: the
`watch_rx` `Disconnected` arm re-runs every iteration once it fires (the
receiver keeps yielding `Disconnected`), so marking the frame dirty there
unconditionally would spin the redraw at full speed. Guard it on an actual
`watcher_active` transition. In practice the arm is unreachable because
`run_loop` still holds the original `watch_tx`.

A single shared tokio runtime (#0095) is safe to `block_on` from many threads at
once, but only because it is multi-thread. `Runtime::block_on` takes `&self`, so
the background action threads and the two watcher threads can each drive their
own future on their own OS thread concurrently, using the one shared worker pool
and reactor. A `new_current_thread` runtime would not serve this: only one
thread can drive a current-thread scheduler at a time, so concurrent `block_on`
calls contend and spawned tasks can stall until their originating thread drives
again. The nesting panic ("Cannot start a runtime from within the context of
another runtime") never triggers here because every site runs on a plain
`std::thread::spawn` (or watcher) thread, never a tokio worker thread and never
already inside a `block_on`; `oauth2.rs` and `config_cmd/helpers.rs` guard the
genuinely nested cases themselves with `Handle::try_current()`. The runtime is a
`static LazyLock` and is never dropped: nothing relied on per-op runtime teardown
for cleanup (IMAP sockets live on async-std's global reactor and the pooled
session cache; SMTP/Graph close when their futures complete).

Two-phase startup (#0003) rests on one non-obvious detail of the store's
`INTEGRITY_CHECKED` amortisation: the expensive `PRAGMA integrity_check` (~240
ms on a 44 MB file) is what any *first* open of a store file in the process
pays, and `count_all_emails` / `outbox::counts_for_account` are ordinary read
opens that trigger it. So the win is not "skip the check" but "do the first open
off the first-paint path": move the counts read to a background thread and the
integrity check moves with it. The registry is keyed by canonical path and
counts per file, so once the background open validates a file, every later open
in that process (the active mailbox load, a sync, a mutation) trusts the verdict
and skips the walk. The corollary is a sequencing rule: the startup auto-fetch
must be queued *after* `BgResult::AccountOpened`, not up front. Two threads
opening the same never-yet-validated file concurrently both see a registry count
of 0 and both run the full check; sequencing the sync behind the open makes the
first open the only one that walks the file. The engine advisory lock (#0061) is
untouched by any of this: it is taken only by the `pending_ops` drain, never by a
read open, so which thread the open runs on is irrelevant to it. The rebuild /
salvage path (#0066) lives inside `Store::open` and simply runs on whichever
thread opened the store, foreground or background.

Auto-mark-read on open (#0087) needed no new mutation path: the `MarkAsRead`
action arm and its `set_read_flag` -> `queue_read_flag` -> `apply_set_read` route
already existed in `actions.rs`, fully wired to the durable queue, but nothing
ever dispatched the action. The whole ticket was the *trigger*, not the write.
The trigger lives in the `run_loop` iteration (not in the render pass): the
preview always shows `selected_email()`, so "opening a message" is just the list
cursor landing on a new row, and `App::take_message_to_auto_mark_read` fires once
per open by remembering the last message in `App::auto_read_opened`. Firing from
render was rejected: `refresh_preview_body` runs under `&mut App` at frame top
but a store mutation there mixes read-path and write-path concerns, and the loop
already owns the post-event settling point where the selection is final.

HTML-to-text in the preview (#0091) needed no external tool and no new crate:
`html2text` was already a direct dependency, used at ingest by
`parse::html_to_plain` (`config::plain()`) to flatten HTML for the stored body.
The ticket's "evaluate w3m / lynx / pandoc" premise predated that fact. The real
defect was a double conversion (HTML flattened to plain at ingest, then
re-parsed as Markdown by `wrap_and_style_body` in `preview.rs`), not a missing
renderer. The fix uses the crate's *rich* interface: `html2text::config::rich()`
`.lines_from_read(html, width)` returns `Vec<TaggedLine<Vec<RichAnnotation>>>`
already wrapped to a width, with per-span annotations (`Strong`, `Emphasis`,
`Link`, `Code`, CSS `Colour`) that map one-to-one onto ratatui `Style` in
`style_for_annotations`. The "outer" annotation comes first in the Vec, so
folding left-to-right layers inner over outer (a `<strong>` inside a link keeps
the underline and adds bold). `config::rich()` defaults `include_link_footnotes`
to false, so links render as their visible text (no `[1]` markers); the `b`/`tb`
browser hatch stays the full-fidelity path. Sourcing the HTML at preview time
reuses `store::read::load_html`, which parses the raw RFC822 on the IMAP path
(no `html` blob there, unlike Graph), so it costs one MIME parse per selection
change; it is memoised in `PreviewHtml` (paid on cursor moves, never per frame).
A per-row `has_html` flag would let it skip plain-only mail the way the
inline-image refresh skips attachment-less rows.
