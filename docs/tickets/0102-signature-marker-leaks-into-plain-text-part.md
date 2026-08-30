---
id: 0102
title: The literal {{SIGNATURE}} marker rides along in the text/plain MIME part
type: bug
priority: next
created: 2026-08-30
status: done
---

The `text/plain` alternative of a sent reply or forward is `draft.body_markdown` verbatim (`src/send.rs`, the `MultiPart::alternative` builder), and nothing replaces the `{{SIGNATURE}}` placeholder there: only the HTML path consumes it (`markdown_to_html`, `src/send.rs:167`), and the TUI preview substitutes it for display only (`src/tui/ui/preview.rs:67`).
So a recipient whose client renders the plain part sees the literal text `{{SIGNATURE}}` in the middle of the message.
Pre-existing before #0099 (found during its review); most clients prefer the HTML part, which is why it went unnoticed.

## Scope

1. Strip or substitute the `{{SIGNATURE}}` marker when building the `text/plain` part, collapsing the surrounding blank lines so the plain text reads naturally.
2. Same treatment anywhere else `body_markdown` leaves the program as message text (invite plain part, if applicable).
3. A test that a sent reply's plain part carries no `{{SIGNATURE}}` literal.

## Acceptance criteria

- The plain-text part of a reply/forward built from a draft containing the marker has the marker removed and no double blank line where it stood.
- The HTML splice behaviour is unchanged.

## Resolution

Added `strip_signature_marker` in `src/send.rs`, which removes the `{{SIGNATURE}}` marker and collapses the blank lines it padded down to a single paragraph break.
`build_draft_message` now runs the plain-text body through it on both the non-invite `text/plain` alternative and the invite plain part, so the marker never reaches a recipient.
The HTML path (`markdown_to_html`) and the TUI preview substitution are untouched, and the signature text spliced into the body at draft time (#0099) stays.
The Graph send path ships HTML only, so it never carried the marker.
Pinned by three tests in `src/send.rs`: `strip_signature_marker_collapses_the_blank_lines_it_padded`, `strip_signature_marker_is_a_no_op_without_the_marker`, and `a_sent_reply_plain_part_carries_no_signature_marker`.
