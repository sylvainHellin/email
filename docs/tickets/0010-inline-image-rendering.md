---
id: 0010
title: Inline image rendering in preview
type: feature
priority: later
status: done
created: 2026-05-01
---

Render inline images in the terminal preview pane using sixel, the iTerm2 inline-image protocol, or the Kitty graphics protocol.

## Notes

- Detect terminal capability at startup (env vars / responses to query sequences).
- Fall back to a `[image: filename]` placeholder when the terminal does not support graphics.
- Only render image attachments referenced inline (`Content-ID` matched in the HTML body); do not auto-render every attached image.

## Shipped (2026-08-13)

Protocol: `ratatui-image` 9 (the version pinned to our ratatui 0.29), whose
capability query picks kitty, iTerm2 or sixel from the terminal's own answer.
There is no hand-rolled escape sequence anywhere in the tree and no protocol
guessing from `$TERM`: the terminal is asked, once, in `tui::images::init`,
after the alternate screen is up and before the first key is read.

Degradation, in three layers, none of which can emit a stray byte:

1. A terminal that answers "no graphics" (or does not answer) leaves
   `ratatui-image` at its halfblocks default, and that default is mapped to
   *no images*. Halfblocks were rejected deliberately: the ticket asks for a
   `[image: filename]` placeholder, and a block of coloured cells is not the
   experience the pane had before this ticket.
2. A Graph row carries no RFC822 (#0042), so the MIME walk has nothing to walk
   and the row shows placeholders only.
3. An undecodable, oversized (> 8 MB or > 40 Mpx) or unsupported-format part is
   dropped to a placeholder, per image, with the rest of the message intact.

Selection follows the ticket: only image parts the HTML body references with a
`cid:` URL are rendered, matched on the `Content-ID` with an id-boundary check
so `cid:logo` cannot match `logo2`. Everything else stays an attachment.

Placement is a block appended after the body text rather than a splice into it.
`html2text` drops the `<img>` before the preview ever sees the text, so there
is no honest anchor to splice at; each image gets a `[image: name]` line and,
where the terminal can draw, the rows it needs reserved blank beneath it. An
image is painted only when its whole block is inside the pane, because a kitty
or sixel image is painted over the cell grid and a half-scrolled one would
spill past the border instead of clipping the way text does.

Performance: one raw-blob read, one MIME walk and one decode per referenced
image, once per cursor move, memoised in `App::preview_images` exactly like the
body and the invite card. A row with no attachments costs a bool test; an HTML
body with no `cid:` in it costs one `contains`. Nothing is decoded per frame,
and `ratatui-image` re-encodes only when the target rect changes.

Not shipped, deliberately: no image caching across cursor moves (the memo holds
one message), no animation, and no rendering inside the attachment picker.
