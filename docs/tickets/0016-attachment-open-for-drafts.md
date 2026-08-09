---
id: 0016
title: Open attachments for drafts (`o`)
type: feature
priority: later
status: done
created: 2026-05-01
---

`o` currently opens attachments only on inbox / archive emails. For drafts, `o` should open the files referenced in `attachments:` frontmatter so the user can verify what's about to be sent.

## Notes

- Reuse `parse::list_attachments()` and `parse::open_file_with_system()`.
- For drafts, attachments are absolute paths in frontmatter, not in a `_attachments/` directory next to the `.md`. The picker already handles multi-file selection.
- Available in List, Headers, Preview focus, same as the existing `o` flow.

## Reconciliation (2026-08-11)

The Notes were written before the data-layer rebuild: `parse::list_attachments()` no longer exists (received-mail attachments are blobs since #0038/#0052, materialised by `store::read::materialise_attachments`). The machinery actually reused is the one that replaced it: `tui::actions::cursor_attachment_files` -> `App::present_attachments` -> the picker -> `Action::OpenAttachment` -> `parse::open_file_with_system`.

## Resolution (2026-08-11)

`cursor_attachment_files` branches on the cursor row: a draft answers from `draft_attachment_files`, everything else keeps the blob materialisation unchanged. The draft branch resolves the indexed draft file through the existing `cursor_draft`, parses its frontmatter and returns the paths in `attachments:`, `~` expanded the way `send::draft_attachments` expands it, so a draft that sends is a draft that opens.

Nothing is copied: a draft's attachments are already files (the forward builder's stable per-account mirror, #0006, or a path the user typed), so `o` opens the very bytes that will be sent rather than a temp rendition. Everything above the helper is untouched, which is why one file, one picker, one keypress: zero, one and many files behave exactly as they do for received mail, and `O` (save) works on a draft for free because it shares the helper.

A listed path that is not on disk is named on the status line rather than skipped silently -- a stale entry is precisely what `o` is pressed to find out about, and it is the failure `mp send` would hit later. Some missing is a warning beside the files that are there; all missing is an error, not an empty "No attachments".

No key binding changed (the `o` / `O` rows already exist in List, Headers and Preview), so the help overlay, `mp dump-keys` and the website table are untouched, and no golden frame moves: the change is reachable only by pressing the key.

## Acceptance

- `o` on a draft opens the files in its `attachments:`, in List / Headers / Preview focus. **Met** -- `a_draft_answers_the_attachment_key_from_its_own_frontmatter`; the three focuses share `execute_list`, so one binding table row serves all three.
- No attachments is not an error. **Met** -- `a_draft_without_attachments_resolves_to_an_empty_list` ("No attachments" on the status line).
- A stale path is surfaced. **Met** -- `a_missing_draft_attachment_is_named_on_the_status_line`.

## Not done (deliberate)

The Drafts list shows no paperclip for a draft that carries attachments: `has_attachments` is false for every draft entry. That is a visible-UI change with golden frames attached and is not what this ticket asked for; filed as a follow-up note here rather than smuggled in.
