---
id: 0097
title: Type a short body in the compose wizard without opening $EDITOR
type: feature
priority: next
status: done
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

## Resolution

Added a multi-line `Body` field to the compose wizard, after `Subject` and last in Tab order.

- `ComposeField::Body` (`src/tui/app/types.rs`) with `next`/`prev` ordering `Subject -> Body -> To`; `ComposeWizard.body: String` plus `has_body_field`/`next_field`/`prev_field`, which restrict the body to a `New` compose and skip it in `Forward`/`EditDraft` navigation (those keep opening `$EDITOR` / rewriting headers in place).
- Key handling (`src/tui/app/keys.rs`, `handle_compose_wizard_key`): `Enter` on the body inserts a newline; `Enter` on `Subject` advances to the body in a `New` compose (still submits in Forward/EditDraft, where Subject is last); typed characters accumulate into the body. Submit from anywhere stays `Ctrl+g`.
- Draft writing (`src/tui/actions.rs`): `write_new_draft_from_wizard` appends a non-empty (trimmed) body after the frontmatter fence; `submit_compose_wizard` skips the `$EDITOR` hand-off when the body is non-empty and lands the draft in Drafts directly (still `status: draft`, approve-before-send). An empty body is unchanged: it opens `$EDITOR` on the draft.
- Rendering (`src/tui/ui/compose.rs`): the bottom area hosts the inline body editor (label, wrapped text, cursor block, empty-state hint) while `Subject`/`Body` is focused in a `New` compose, and the contact suggestions while an address field is focused. Forward/EditDraft keep the old Subject placeholder.

Key mapping vs #0092: the wizard's internal keys (`Tab`, `Enter`, `Ctrl+g`, `Ctrl+u`, `Ctrl+p`/`Ctrl+n`) are overlay-local and are not in the `KEYMAP` registry, so `mp dump-keys --json` and `website/src/data/tui-keys.json` are unchanged and no website key page needed updating. The submit key is the pre-existing `Ctrl+g` force-submit; no binding was added, renamed or removed.

Also fixed the adjacent Subject-field hint mislabel ("Tab: prev field", UX audit §b.2/§c.12) as a byproduct of reworking the hint bar for the new field order; the hints now read correctly per field.

Tests: `wizard_body_is_written_into_the_draft_file`, `wizard_empty_body_leaves_the_draft_body_blank` (`src/tui/actions.rs`); `compose_wizard_tab_reaches_the_body_field`, `compose_wizard_enter_on_body_inserts_a_newline`, `compose_wizard_ctrl_g_submits_from_the_body` (`src/tui/app/keys.rs`); golden frame `golden_compose_wizard_with_body` (`src/tui/ui/golden_frames.rs`).
