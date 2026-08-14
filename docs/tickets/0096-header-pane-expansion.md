---
id: 0096
title: Expand the header pane with Bcc/Reply-To, an attachment indicator, and bounded scroll
type: feature
priority: next
status: open
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
