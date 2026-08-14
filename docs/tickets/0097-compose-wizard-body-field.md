---
id: 0097
title: Type a short body in the compose wizard without opening $EDITOR
type: feature
priority: next
status: open
created: 2026-08-14
---

The compose wizard has only To/Cc/Bcc/Subject fields and no body field (UX audit §c.7, §b.2).
Every message, even a one-line reply, forces the external editor round-trip: `n` opens the wizard, submit launches `$EDITOR` on the draft `.md`, edit the body, save and quit (`submit_compose_wizard` -> `edit_new_draft`, `src/tui/actions.rs`).
For a short message the editor launch is the dominant cost of the whole compose flow.

## Scope

Add an optional body field to the compose wizard so a short message can be typed and submitted without `$EDITOR`.
The field is multi-line and the last field before submit.
Leaving it empty and submitting keeps the current behaviour: open `$EDITOR` on the draft so a longer body can be written there.
A non-empty body is written into the draft `.md` body directly, and the draft lands in Drafts the same way it does today (still `draft` status, still approve-before-send).

The `$EDITOR` path stays available for anything the inline field is awkward for (long bodies, references to other mail); the inline field is the fast path, not a replacement.

## Cross-references

- Compose flow and the approve-then-send tail: [#0089](0089-send-current-draft-approve-and-send.md) shortens the post-editor steps; this ticket removes the editor step itself for short mail.
- The wizard's "Tab: prev field" mislabel on the Subject field (UX audit §b.2, §c.12) is adjacent and worth fixing in the same pass, but is a separate defect.

## Acceptance criteria

- The compose wizard exposes a body field; a message typed there is saved into the draft body with no `$EDITOR` launch.
- Submitting with an empty body opens `$EDITOR` on the draft, unchanged from today.
- A draft created via the inline body follows the normal Drafts lifecycle (approve, then send).
