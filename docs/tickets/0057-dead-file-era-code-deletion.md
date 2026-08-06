---
id: 0057
title: Delete the dead file-era code (draft picker, path helpers, SaveFrontmatter, mailbox dirs)
type: chore
priority: next
status: open
created: 2026-08-06
---

From the architecture review synthesis, Tier 2 item 5: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: S, one session.

Code the cutover orphaned but did not remove.
Each item is individually trivial; they are batched because removing one exposes the next, and because the endpoint is worth stating: after this plus [#0053](0053-contacts-rebuild-data-loss.md), the crate touches the filesystem only for drafts, blobs and attachment materialisation.

## Evidence

- `src/draft.rs:51` `select_inbox_email` walks an inbox directory to let the user pick a message; nothing calls it.
- `src/parse.rs:330` `attachments_dir_for`, `:371` `stable_attachments_dir`, `:380` `account_dir_for_email` and `:386` `list_attachments` all take or derive a `.md` path.
- `src/types.rs:176` `SaveFrontmatter` is constructed only by its own test at `types.rs:326`.
- `src/config.rs:836` `resolve_mailbox_dir` maps a mailbox name to a directory that is never created, kept alive by its own tests at `config.rs:1236-1295`.
- Write-only fields: `sent_dir`, `archive_dir` and `inbox_dir` on `AccountState` (`src/tui/app/types.rs:662-666`, populated at `:713-763`) and on `App` (`src/tui/app/mod.rs:88-92`, assigned at `:375-379`), never read.
- The dead schema columns of the same era are handled in [#0054](0054-schema-bump-bundle.md), not here.

## Scope

1. Delete the functions, the struct and the fields listed above, plus the tests that exist only to exercise them.
2. Delete any import or helper left unreferenced by those removals.
3. Verify with `cargo build` warnings and a `rg` sweep that no `.md`-path helper survives outside the draft path.

## Acceptance criteria

- `cargo test` green and `cargo build` clean of dead-code warnings for the touched modules.
- `rg 'mailbox_dir|account_dir_for_email|SaveFrontmatter' src/` returns nothing outside the draft and blob paths.
- No behaviour change: the diff is deletions plus the imports they orphan.
