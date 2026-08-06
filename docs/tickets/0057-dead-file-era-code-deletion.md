---
id: 0057
title: Delete the dead file-era code (draft picker, path helpers, SaveFrontmatter, mailbox dirs)
type: chore
priority: next
status: done
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

## Resolution

Deleted, each re-checked for a live caller first: `draft::select_inbox_email`; `parse::attachments_dir_for`, `parse::account_dir_for_email`, `parse::list_attachments` and `parse::parse_email_date_prefix`; `types::SaveFrontmatter`; `config::resolve_mailbox_dir`; and `sent_dir` / `archive_dir` / `inbox_dir` on both `AccountState` and `App`.
The 14 tests that existed only to exercise them went with them, which is the whole of the 820 -> 806 suite delta.
No dependency fell out: `gray_matter` still has the drafts parser (`draft::parse_email_draft`), and `walkdir` still has `store::drafts`, `store::blobs` and `draft::find_drafts`.

`open_store` now lives in `src/store/mod.rs` beside `Store::open`, unchanged, and its doc says outright that it is the opener that never creates a file (which is what `Store::open_account` does).
Every call site imports `crate::store::open_store` (seven modules, sixteen calls); `contacts/extractor.rs` and `dump.rs` no longer reach into `crate::tui` for it.

The three stale strings are corrected: `mp config show` prints the store, blob and drafts paths under `[local paths]` and a bare role-to-server mapping under `[mailboxes]` instead of a mail directory nothing creates; the `src/graph.rs` module comment names ingest and the store as where its return types land; the `mp --help` about string is "A terminal email client: Markdown drafts on disk, received mail in a local store" (CLI help snapshot updated to match).

### Kept, with reasons

- `parse::stable_attachments_dir`, listed in the evidence, is live: `draft.rs` materialises forward attachments through it.
- `types::InboxFrontmatter` still has consumers.
  Its production caller went with `select_inbox_email`, but the `draft::set_event_rsvp` and `draft::set_event_attendee_status` tests parse their rewritten invite back through it.
  Those two frontmatter rewriters are themselves file-era leftovers with no production caller (RSVP state is derived by `reconcile::own_rsvp` now), so they and the type should go together, which is a deletion this ticket did not scope.
  The type carries a comment saying so.
- `config::mailbox_dir` survives because `MailboxInfo.dir` is still a `PathBuf` built from it, so the `rg 'mailbox_dir|account_dir_for_email|SaveFrontmatter'` criterion holds for the last two names only.
  That identity change is [#0064](0064-identity-type-cleanup.md).
- `dump.rs` still imports `build_mailboxes` and `resolve_date` from `crate::tui::app`, so "no library module imports from `crate::tui`" is not met in full.
  Both are display helpers entangled with `MailboxInfo` and the `resolve_date` split (review finding F3), so moving them is that refactor, not this deletion pass.
- `Cargo.toml`'s `description` still carries the old about string.
  Out of the scope this ticket listed; one line for whoever touches packaging next.
