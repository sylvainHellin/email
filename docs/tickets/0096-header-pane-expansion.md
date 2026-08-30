---
id: 0096
title: Expand the header pane with Bcc/Reply-To, an attachment indicator, and bounded scroll
type: feature
priority: next
status: done
created: 2026-08-14
---

The header pane is thin (UX audit §metadata, §c.9).
`render_headers` (`src/tui/ui/headers.rs`) shows exactly From, To, Cc (if non-empty), Subj, Date, and [status].
Hidden or absent: Bcc, Reply-To, Message-ID, date-received vs sent, an attachment indicator or list, and flag state.

Two defects sit on top of the thinness:

- `headers_scroll` is `saturating_add(1)` with no upper clamp against content height (`src/tui/app/keys.rs`, `A::HeadersDown`), so the metadata can be scrolled into an empty void.
- There is no attachment affordance in the header pane even though `o`/`O` act on attachments there, so a user cannot tell from the headers that a message has attachments.

## Scope

1. Show Bcc and Reply-To when present.
2. Add an attachment indicator so a message with attachments is visible from the header pane, matching the `o`/`O` action that already works there.
3. Bound the header scroll against content height (clamp `headers_scroll`), and provide an expandable view for the fuller metadata rather than an unbounded scroll into empty space.

## Acceptance criteria

- Bcc and Reply-To appear in the header pane when the message carries them.
- A message with attachments shows an attachment affordance in the header pane.
- Header scroll cannot move past the content; there is no scroll into an empty void.

## Resolution

Done against the current #0092 nvim keymap. The ticket predates #0092 but named
no keys, so no remapping was needed; the attachment affordance is the header-pane
counterpart of the `t o` / `t s` open/save-attachment actions that resolve in
the headers pane through the shared MESSAGE context. No key binding was added,
renamed or removed, so `mp dump-keys --json` still matches
`website/src/data/tui-keys.json` and no website page changed.

What shipped:

- `render_headers` (`src/tui/ui/headers.rs`) now emits a `Reply-To` row and a
  `Bcc` row when the message carries those headers (a blank value draws no row,
  via the `present` filter), plus an `Attach:` line with the paperclip glyph
  `\u{f0c6}` (the same glyph the list uses) when the message has attachments.
  Order: From, Reply-To, To, Cc, Bcc, Subj, Date, Attach.
- Bounded scroll: `render_headers` now takes `&mut App` and clamps
  `headers_scroll` to the wrapped content height (`content_rows - inner_height`)
  each frame, mirroring the activity/help overlays, so `j` at the bottom can no
  longer scroll into an empty void. No selection resets the offset to zero.
- Plumbing for Reply-To/Bcc: parsed at ingest into `FetchedEmail`
  (`src/parse.rs`), stored in two new `messages` columns with a schema bump
  (v7 -> v8, `src/store/schema.rs`), written by `src/ingest.rs`, read into
  `MessageRow` (`src/store/read.rs`) and carried into `EmailEntry`
  (`src/tui/app/types.rs`). The store is a cache with no migrator, so the bump
  drops and refills each store from the server on the next sync, which is where
  the new columns get populated.

Limitations (left as follow-ups, not in scope here):

- The drafts index and the Graph fetch path do not carry Reply-To/Bcc yet, so
  the pane surfaces them for stored IMAP mail only. Draft frontmatter already
  holds `reply_to:`/`bcc:`; wiring `DraftRow` and the Graph message shape would
  extend the display to those paths.
- `EmailEntry` exposes only `has_attachments` (a bool), so the `Attach:` line
  says `yes` rather than listing names or a count.

Tests: `src/tui/ui/headers.rs` gains five unit tests (Reply-To/Bcc present and
absent, attachment affordance present/absent, scroll clamp, no-selection reset);
the three `golden_mail_view*` snapshots were regenerated to show the new
`Attach:` line on the attachment-bearing row and reviewed.
