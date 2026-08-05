---
id: 0049
title: Pre-nuke oracle capture: golden frames, gap-list fixtures, envelope dumps, freeze
type: chore
priority: now
status: open
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

## Acceptance criteria

- The golden-frame snapshots are committed, two consecutive runs are identical, and no test reads a live account.
- Every gap-list fixture is green, or is tagged `known-bug` with its target behaviour written down.
- The envelope dump is deterministic across two runs on an unchanged mailbox, and its record shape is documented well enough for the new build to reimplement it from the store.
- `mp-legacy --version` runs and reads the existing tree, and the `pre-dal-nuke` tag exists.
- The two binaries never point at the same account data directory. This is a standing rule, not a one-off check.
- `cargo test` green and `cargo install --path .` clean.

## Unblocks

- [#0037](0037-sqlite-store-engine-skeleton.md), and through it every other ticket in the arc.
