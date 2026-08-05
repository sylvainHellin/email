---
id: 0049
title: Pre-nuke oracle capture: golden frames, gap-list fixtures, envelope dumps, freeze
type: chore
priority: now
status: done
created: 2026-07-31
---

Stage 0 of the data-access-layer redesign.
Plan: [data-access-layer](../plans/data-access-layer.md), decisions D and E.
Foundation plan: [2026-07-31_dal-foundation-plan-v2](../../.agents/handoff/2026-07-31_dal-foundation-plan-v2.md), units 0a to 0d and section 2.
Backing audit: [test-suite-quality-audit](../../.agents/research/test-suite-quality-audit.md).

The complete nuke voids the byte-identity oracle: there is no `.md` write left to compare the new build against.
This ticket captures the replacement oracles from the current build, on `main`, before any greenfield code exists.
It must be closed before [#0037](0037-sqlite-store-engine-skeleton.md) opens.

## Scope

1. Golden frames (unit 0a). A `frame_snapshot(app, w, h)` helper renders through `ui::view` on a `TestBackend` and emits the text rows plus a compact per-row style-run legend, since ratatui's buffer `Display` drops colours. Lean scope, deliberately: the mail view (sidebar, list, preview) and the calendar at 120x40, plus the help overlay. The style-run legend is captured only where style carries meaning (unread bold, the cursor row, selection); there is no multi-size sweep and no cosmetic snapshot, because a snapshot that cannot fail on a real regression is churn. The fixture corpus has fixed dates, a unicode subject, a read and unread mix, an invite and an attachment, and it pins the theme explicitly, because `App::new` reads the theme from the user's global config and an unpinned frame would depend on the developer's machine. These are lib unit tests under `src/tui/ui/`, since `App::default_for_tests` is `#[cfg(test)]`.
2. Gap-list fixtures (unit 0b). The tests the audit says have no oracle today: RFC 2047 encoded-word subjects and display names, non-UTF-8 body charsets (ISO-8859-1, windows-1252, Shift_JIS), malformed MIME (truncated multipart, missing boundary, nested `message/rfc822`), mailbox and unread counts for `count_all_emails`, `resolve_send_account` and `fetched_to_email_entry` in `src/tui/helpers.rs`, and a snapshot of `mp --help` plus each subcommand's `--help`. Every fixture is tagged `parity` (the new build must match the recorded behaviour) or `known-bug` (the recorded behaviour is wrong and the written target is the expectation). The tag is mandatory: RFC 2047 handling is entirely unverified today, and an untagged capture would launder a bug into a contract.
3. Envelope dump (unit 0c). Add `mp dump-mailbox --json` to the current build, emitting one normalised, path-free record per message (account, mailbox, message-id, from, to, cc, subject, date_sort, flags, attachment names and sizes, invite flag), stably sorted. Landing this small additive command on `main` before the freeze is approved. Capture dumps for every real account into a git-ignored local directory, since they contain real mail metadata.
4. Freeze (unit 0d). `cp "$(which mp)" ~/.local/bin/mp-legacy`, tag the tree `pre-dal-nuke`, and record both in [data-access-layer](../plans/data-access-layer.md).

## Unit 0b capture notes

Where the fixtures live:

- [tests/mime_oracle_integration.rs](../../tests/mime_oracle_integration.rs): RFC 2047 encoded-word headers, non-UTF-8 body charsets, malformed MIME.
- [tests/cli_help_snapshot.rs](../../tests/cli_help_snapshot.rs): one insta snapshot of `mp --help` and all 37 subcommand help screens, captured by running the built binary (`--help` maps to clap's short help for commands with no long help and to the long help for `mp dump-keys`, so an in-process `render_long_help` would record a layout no user sees).
- `count_all_emails` in the `src/tui/app/types.rs` test module, `resolve_send_account` and `fetched_to_email_entry` in a new test module in `src/tui/helpers.rs`.

Every test carries a `parity` or `known-bug` tag in its doc comment.
The `known-bug` captures, with their targets:

- Raw 8-bit header bytes decode as strict ISO-8859-1, so 0x80..0x9F become C1 control characters, while the same bytes inside an `=?ISO-8859-1?...?=` encoded-word and in a body decode as windows-1252.
  The three paths disagree; the windows-1252 mapping is the right one.
- A multipart with no `boundary=` parameter, or with a boundary that never appears in the body, yields an entirely empty body: the message renders blank.
  The raw entity body must stay reachable.
- A nested `message/rfc822` part is dropped whole: not extracted into the body, not listed as an attachment.
  Forwarded-as-attachment mail is invisible.
- `resolve_send_account` matches the draft's `from:` against the account address with `contains`, so `not-sylvain@work.example` resolves to the `sylvain@work.example` account, and an account with an empty `default_from` swallows every draft.
  Compare parsed addresses for equality instead.
- `fetched_to_email_entry` keeps the sender-local wallclock in `date_sort` (the #0024 fix never reached this path) and keeps the raw `From:` header where the on-disk path stores the display name only.
- `count_all_emails` counts files, not listable messages: a `.md` file that is not valid UTF-8 is counted but is dropped by `load_emails`, so the sidebar over-counts.

Named gaps, not captured:

- Unread counts.
  No unread-count function exists in the current build, so there is no behaviour to be at parity with; the sidebar number is a total and the test records only that.
  The new build's unread count is new behaviour, to be specified rather than reproduced.
- `lib_do_sync`, `lib_do_multi_search` and `ensure_search_result_saved` in `src/tui/helpers.rs` remain untested: the first two need a live IMAP session, and all three mutate `App` through the background-result path.
  Out of scope for this unit, which was limited to the two pure helpers.
- Server-side flag round-trip, `open_file_with_system` and `copy_to_clipboard` (audit gap-list) stay untested: they need a network peer or a desktop session, so neither is an offline oracle.

## Unit 0c capture notes

`mp dump-mailbox --json` ([src/dump.rs](../../src/dump.rs)) emits NDJSON on stdout: one compact JSON object per message, one per line, LF-terminated.
Every configured account by default; `-A/--account` restricts it to one, `--mailbox` (repeatable, matching the role, the slug or the sidebar label, case-insensitively) restricts the mailboxes.
`--json` is a required flag rather than a default, so a later format cannot silently change what a recorded dump means.

The record, in serialized field order: `account`, `mailbox`, `message_id`, `from`, `to`, `cc`, `subject`, `date_sort`, `flags`, `attachments` (objects of `name` and `size`), `invite`.
`mailbox` is the role or slugified server name (`inbox`, `drafts`, `sent`, `archive`, the slug of an extra mailbox), never a path.
`message_id` and the four header fields are the frontmatter values verbatim, `null` when absent: a missing Message-ID is recorded as missing, because synthesizing an identity is the new stack's behaviour and recording it here would launder it into the oracle.
`date_sort` is the current build's sort key, from `tui::app::resolve_date`.
`flags` is a sorted list from the closed set `approved`, `draft`, `seen` (`seen` from `read: true`, the other two from `status:`); the current build tracks no other per-message flag locally.
`attachments` names come from the `attachments:` frontmatter list, reduced to their file name, with `size` read from the sibling `<stem>_attachments/` directory and `null` when that file is absent.
`invite` is true when the file carries an `event:` block.

Records are sorted by `(account, mailbox, date_sort, message_id, subject, file name)`; the file name is the final tiebreaker and is never emitted, which keeps the order total without putting a path in the output.
The contract lives in [tests/dump_mailbox_integration.rs](../../tests/dump_mailbox_integration.rs) as the exact expected NDJSON of a fixture tree, plus a determinism check, all tagged `parity`.

Two findings from the real capture: outgoing mail records the *source path* of an attachment in `attachments:` (`/tmp/audio/briefing.mp3`, and one under `/home/sylvain`), which is why names are reduced to their file name; and three messages across the three accounts have no Message-ID at all, so identity-less mail is not hypothetical.

## Unit 0d freeze notes

The three capture units landed on `main` first: golden frames in 55eb172 (unit 0a), gap-list oracles in 6bb764d (unit 0b), the `mp dump-mailbox --json` command in f15ed7d (unit 0c), with the real-account dumps taken at that point into the git-ignored `dumps/` directory.
Unit 0d is this freeze, executed on the commit that closes the ticket.

What the freeze leaves behind:

- The tag `pre-dal-nuke`, on the commit that closes this ticket, which is the last files-as-truth commit.
- `~/.local/bin/mp-legacy`, copied from the cargo-installed `mp` built at that commit.
- The real-account envelope dumps in the git-ignored `dumps/` directory, one file per account (`dumps/pre-nuke-<account>.ndjson`): 312 records for assistant, 627 for tum, 450 for perso.
  They stay out of git because they carry real mail metadata.

`mp-legacy` and the rewritten `mp` must never point at the same account data directory: the new build's wipe-and-resync deletes the `.md` tree the old build treats as truth.

## Acceptance criteria

- The golden-frame snapshots are committed, two consecutive runs are identical, and no test reads a live account.
- Every gap-list fixture is green, or is tagged `known-bug` with its target behaviour written down.
- The envelope dump is deterministic across two runs on an unchanged mailbox, and its record shape is documented well enough for the new build to reimplement it from the store.
- `mp-legacy --version` runs and reads the existing tree, and the `pre-dal-nuke` tag exists.
- The two binaries never point at the same account data directory. This is a standing rule, not a one-off check.
- `cargo test` green and `cargo install --path .` clean.

## Unblocks

- [#0037](0037-sqlite-store-engine-skeleton.md), and through it every other ticket in the arc.
