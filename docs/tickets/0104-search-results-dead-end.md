---
id: 0104
title: Search overlay results are a dead end
type: bug
priority: now
status: open
created: 2026-08-20
---

Reported by Sylvain 2026-08-20: after an `ff` search returns hits, "I can't do anything with them: I can't open it, fetch it, cp the path to the md file".

## What actually exists today

The result list does have actions (`keys.rs::handle_search_overlay_list_key`): `Enter`/`e` open read-only in `$EDITOR`, `r`/`R` reply, `w` forward, `a` archive, `b` open HTML in browser, `o`/`O` attachments.
Focus even jumps to the list automatically when hits arrive (`bg.rs`, `count > 0`).
They are still effectively invisible and incomplete:

1. Discoverability: the overlay footer always shows the *form* hints (`Tab/Shift+Tab: fields | Space: toggle | Enter: search | Esc: close`) or the status line ("N results"), never the list keys.
   Unless you open `?` there is no way to know the list is actionable.
2. No path access: there is no way to yank the `.md` path of a hit that resolved to a local store row.
3. No fetch/materialise: a hit that does not resolve locally (server-only message) cannot be pulled into the store; only its fetched body is viewable.
   Cross-ref #0085 (on-open body refetch), which builds the same fetch plumbing.
4. No jump-to-list: you cannot close the overlay positioned on the hit in the normal message list (when the hit is local), which is what "open it" usually means in Apple Mail / Outlook.

## Proposed scope

- Footer swaps to the list-key hints whenever `server_search_focus == List` (keep the result count in the title or prepend it).
- `y`: yank the store `.md` path to the clipboard for a locally-resolved hit; status-line error for a server-only hit.
- `f` (or reuse `Enter` semantics): fetch a server-only hit into the store, then treat it as local.
- `Enter` on a local hit: close the overlay and select the message in the list view (scope switch to its mailbox if needed); keep `e` as the read-only editor open.

## Acceptance

- All list actions advertised in the footer while the list has focus.
- A local hit can be opened in the list view, yanked, replied, forwarded, archived.
- A server-only hit can be fetched into the store from the overlay.
