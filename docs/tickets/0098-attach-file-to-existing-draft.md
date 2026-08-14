---
id: 0098
title: Attach a file to an existing draft from the TUI
type: feature
priority: next
status: open
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
