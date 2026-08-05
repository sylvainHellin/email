# Lessons learned

Non-obvious gotchas, regressions, and hard-won fixes that are easy to forget. Append a new entry whenever you spend more than ~30 minutes discovering something that was not obvious from the code.

Format: short imperative title, one-paragraph description, and (when useful) a code reference.

## `cargo install --path .` leaves a stale `email` binary after the `mp` rename

The CLI binary was renamed `email` -> `mp` (ticket #0022). `cargo install --path .` installs `~/.cargo/bin/mp` but does **not** remove the previously installed `~/.cargo/bin/email`, so a stale old binary lingers and shadows nothing but confuses `which email`. On this dev machine the fix is a symlink so old muscle memory / scripts keep working: `rm -f ~/.cargo/bin/email && ln -s mp ~/.cargo/bin/email` (relative target, resolves within `~/.cargo/bin`). This is a local convenience only -- it is not created by any install step and does not ship anywhere. The Cargo package/library are still named `email` internally; only the `[[bin]]` target changed, so `use email::...` imports are unaffected.

## HTML charset must be injected before saving

Incoming HTML bodies are saved as raw bytes from the server. Browsers default to latin-1 when no charset is declared, breaking umlauts and other non-ASCII characters. Always inject `<meta charset="UTF-8">` before writing to disk -- see `ensure_utf8_charset()` in `parse.rs`.

## Signature placement uses a placeholder, not a trailing append

Reply and forward drafts contain a `{{SIGNATURE}}` placeholder between the reply area and the quoted conversation. `markdown_to_html` replaces it at send time so the signature lands between reply and quote. If the placeholder is removed, the signature falls back to end-of-body. Do not "simplify" by always appending -- it will land below the quoted thread.

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
