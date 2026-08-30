---
id: 0098
title: Attach a file to an existing draft from the TUI
type: feature
priority: next
status: done
created: 2026-08-14
---

There is no key to attach a file to an existing draft from the TUI (UX audit §c.8, §b.4).
The only automated attach is "send contact as vCard" (`v` in Contacts, which builds a new draft); attaching to an existing draft was explicitly deferred (comment at `src/tui/actions.rs`).
Today the user must hand-edit the `attachments:` YAML frontmatter of the draft `.md` in `$EDITOR`, then trust that the paths resolve at send time.

## Scope

Add a TUI action, on a Drafts row, that prompts for a file path and appends it to the draft's `attachments:` frontmatter.
Reuse the send-time path handling so a draft that attaches is a draft that sends: `send::draft_attachments` already expands `~` and resolves the listed paths, and [#0016](0016-attachment-open-for-drafts.md) resolves and opens those same paths via `draft_attachment_files`.
A path that is not on disk is surfaced on the status line rather than silently accepted, matching the stale-path handling #0016 landed for `o`/`O`.

The picker is a path prompt (there is no GUI file dialog); tab-completion of the path is a nice-to-have, not required for a first cut.

## Cross-references

- [#0016](0016-attachment-open-for-drafts.md) (done) resolves and opens a draft's existing attachments; this ticket adds them.
  The two share the draft-frontmatter path logic.
- [#0006](0006-attachment-paths-after-archive.md) established the stable per-account attachment mirror the forward builder uses; a user-attached file may point anywhere, so this ticket does not copy it, it references it, the same as a typed path.
- #0016's deferred note (no paperclip on Drafts rows carrying attachments) is the natural companion display change once drafts can gain attachments here.

## Acceptance criteria

- A Drafts row exposes an attach action that appends a file path to the draft's `attachments:` frontmatter.
- The attached file is the one that sends (same path resolution as `send::draft_attachments`).
- A path that does not exist on disk is reported on the status line, not silently stored.

## Resolution (2026-08-14)

The ticket predates the #0092 nvim-style keymap and named no key; the intent maps onto the `t` thread/attachment family as `t a` (attach), a `KeyCtx::List` + `Guard::DraftsOnly` binding beside `ce` (edit recipients), which shares that family's Drafts-only, list-scoped shape. `KeyAction::AttachFile` (`src/tui/app/keymap.rs`) arms an inline path prompt, `attach_file_input: Option<String>` on `App` (`src/tui/app/mod.rs`), the same one-`Option` state machine as the #0017 jump-to-date prompt and rendered in the same borrowed one-line list slot with an `attach: ` prefix (`src/tui/ui/list.rs`).

`handle_attach_file_key` (`src/tui/app/keys.rs`) expands `~` the way `send::resolve_attachment_paths` does for the existence check but stores the raw text, so a portable `~`-relative entry survives to be re-expanded at send time. A path that is not on disk is named on the status line and the prompt stays armed (a typo is a correction, not a re-arm), matching the jump-to-date prompt and #0016's stale-path handling. An on-disk path queues `Action::AttachFileToDraft`, handled in `attach_file_to_draft` (`src/tui/actions.rs`): it resolves the cursor draft via the existing `cursor_draft` and appends the entry through the new `draft::append_draft_attachment` (`src/draft.rs`), a byte-preserving frontmatter line-surgery function modelled on `rewrite_draft_recipients`. So a draft that attaches here is a draft that sends: the very path stored is the one `send::draft_attachments` reads.

Nothing is copied: the entry references the file wherever it lives (per #0006, a user-attached file may point anywhere), the same as a typed path. The Drafts-row paperclip for a draft that now carries attachments stays deferred (#0016's follow-up note), so `has_attachments` is still false for draft entries and no list golden frame moves for that.

Key-binding surface regenerated: `mp dump-keys --json` -> `website/src/data/tui-keys.json` gains the `ta` row, the help overlay golden (`golden_help_overlay`, 103 -> 104 bindings) was re-accepted, and a new `golden_drafts_view_attach_prompt` frame pins the armed prompt. No CLI command or flag changed, so `mp --help` and the hand-written website pages are untouched.

### Acceptance

- Attach action on a Drafts row appends to `attachments:`. **Met** -- `the_attach_prompt_is_armed_typed_and_committed_by_the_keyboard`, `test_append_attachment_to_a_bare_key`, `test_append_attachment_after_existing_items`, `test_append_attachment_adds_the_key_when_absent`.
- The attached file is the one that sends. **Met** -- the raw path is stored verbatim and `append_draft_attachment` writes the same double-quoted YAML the forward builder does; `send::draft_attachments` reads it back through `resolve_attachment_paths`.
- A path not on disk is reported, not stored. **Met** -- `the_attach_prompt_is_armed_typed_and_committed_by_the_keyboard` asserts the missing-path branch keeps the prompt armed with `No such file` on the status line and queues nothing.
- Drafts-only. **Met** -- `attach_is_refused_outside_drafts`.
