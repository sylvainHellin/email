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

## Also in scope (added 2026-08-06)

Layering, from the fresh-context review of [#0053](0053-contacts-rebuild-data-loss.md):

- `open_store` lives in `src/tui/app/types.rs:320-332` and is documented as the canonical opener in `src/store/mod.rs:125`, yet library code reaches into the TUI module to call it: `src/contacts/extractor.rs` and `src/dump.rs:98`.
  It is a pure helper (path check, `Store::open`, `warn!` on failure, no terminal or app state), so moving it next to `Store` is a rename plus imports.
  `Store::open_account` is not a drop-in replacement: it creates a store file for an account that has never synced, which is the behaviour `open_store` exists to avoid.

Stale text found during [#0056](0056-architecture-docs-rewrite.md) and left alone there because it was out of that ticket's scope:

- `src/config_cmd/show.rs:147`: `mp config show` prints `[mailboxes] [inbox] INBOX -> <account_dir>/inbox`, a local path nothing has created since the cutover.
  Same falsehood class as the wizard blocks #0056 fixed.
- `src/graph.rs:1-6`: the module comment still says the client integrates with "the existing local storage layer (`.md` + `.html` + `_attachments/`)".
- `src/main.rs:23`: the top-level about string, "A CLI tool for sending emails from Markdown drafts with YAML frontmatter", now describes half the product (drafts are Markdown, received mail is not).

## Scope

1. Delete the functions, the struct and the fields listed above, plus the tests that exist only to exercise them.
2. Delete any import or helper left unreferenced by those removals.
3. Verify with `cargo build` warnings and a `rg` sweep that no `.md`-path helper survives outside the draft path.
4. Move `open_store` out of `tui::app` next to `Store`, and update the three call sites.
5. Correct the three stale strings and comments listed above.

## Acceptance criteria

- `cargo test` green and `cargo build` clean of dead-code warnings for the touched modules.
- `rg 'mailbox_dir|account_dir_for_email|SaveFrontmatter' src/` returns nothing outside the draft and blob paths.
- No behaviour change: the diff is deletions plus the imports they orphan, the `open_store` move, and text.
- No library module imports from `crate::tui`.
