---
id: 0052
title: TUI mutation half on the store and the selector
type: refactor
priority: now
status: open
created: 2026-08-01
---

Stage 2c of the data-access-layer redesign, and the third gate ticket.
Plan: [data-access-layer](../plans/data-access-layer.md).

[#0038](0038-read-path-to-db.md) moved the read path and the store-backed mutations, [#0050](0050-selector-contract-drafts-index.md) landed the `mp://` selector contract and the drafts index, and the CLI now does every one of those operations off the store.
The TUI does not.
Its Reply, Forward, Send, Approve, Mark-draft, Edit-recipients, `$EDITOR`, attachment and browser flows still address a message as a `.md` file, so they decline with a status line instead of running.
This ticket ports them onto the substrate their CLI counterparts already use, which is the last thing between the branch and the stop-gate.

Depends on [#0038](0038-read-path-to-db.md) and [#0050](0050-selector-contract-drafts-index.md), both landed.

## Scope

Port every TUI flow below onto the store plus the selector: quote and forward bodies come from `message_blobs` through `store::read::load_body` / `load_html`, draft files are found through the drafts index rather than by walking `drafts/`, and attachments are materialised from `message_blobs` the way `mp save` and `mp open` do.
No flow re-reads a received `.md` file, because there is not one.

1. Reply and Reply-all (list and preview), building the draft with `draft::create_reply_draft_from` over a `SourceMessage` assembled from the store row, HTML companion included.
   Landed, unit A.
2. Forward, the same shape through `create_forward_draft_from`, with attachments materialised from the row's blobs into the stable per-account mirror.
   Landed, unit A: `w` opens the compose wizard in `ComposeMode::Forward` again, as the pre-nuke build did, and the wizard's recipients and subject are written over the builder's before the draft is indexed.
3. Send, whose draft comes from the drafts index and whose account resolver is `helpers::resolve_send_account`, which loses its `#[allow(dead_code)]` here.
4. Approve and the batch approve.
5. Mark-draft and the batch mark-draft.
6. Edit recipients (the compose wizard's `EditDraft` mode) resolved through the drafts index rather than a cached path.
   Landed, unit A: the mode carries the draft's `id:` and resolves it on open and again on submit.
7. Open in `$EDITOR`, which is `mp edit <selector>`'s job done in-process.
8. Attachment open and save, from the list and from the search-result overlay, sourced from `message_blobs`.
9. Open in browser, from the list and from the search-result overlay: the rendered HTML comes from the html blob or the raw blob, not from a `.html` file beside a `.md`.
10. Open event source, from the calendar view, through the invite's own row and ics blob.
11. Search-result Open, Reply and Forward, which are the same three flows over a hit that resolved to a row.
    Reply and Forward landed, unit A, for both halves of a hit: one that resolved builds its source from the row, one that did not builds it from the content the overlay is already rendering (`draft::source_from_fetched`) rather than declining.
    Open is still open.

Housekeeping this ticket owns, so nothing survives by accident:

- Delete `draft::create_reply_draft`, `draft::create_forward_draft` and `draft::source_from_file`, the path-shaped test-only fixtures left over from the file build, and port the formatting tests in `tests/draft_integration.rs` onto `create_reply_draft_from` / `create_forward_draft_from` with a `SourceMessage`. The three file-attachment-hydration unit tests in `src/draft.rs` (#0006 lineage) die with them: the behaviour they cover does not exist post-nuke.
  Done, unit A.
  `parse::link_or_copy` went with them, its only caller having been `source_from_file`, and `main.rs`'s `source_from_row` plus `materialise_attachments` moved into the library (`draft::source_from_row`, `store::read::materialise_attachments`) so the TUI and the CLI build a draft's source through one function.
- Every remaining `needs_tui_mutation_half` decline disappears with the flow it guards; the helper itself goes when the last caller does.

## Acceptance criteria

- No TUI action shows a decline status line for any flow listed above.
- Reply and Forward from the TUI produce the same draft, byte for byte, as `mp reply` / `mp forward` on the same selector, HTML companion included.
- Sending a draft from the TUI and from `mp send <selector>` take the same path through the outbox.
- Attachment open, attachment save and open-in-browser read from `message_blobs` only; nothing looks for `_attachments/` or a `.html` beside a `.md`.
- The golden frames still match, and the in-TUI help overlay plus `website/src/pages/` describe what the build actually does.
- No `#[allow(dead_code)]` remains in `src/tui/` for a flow this ticket lands.

## The stop-gate

The stop-gate of the data-access-layer redesign is reached when #0038, #0050 and #0052 are all done.
#0038 and #0050 made the read path and the CLI store-backed; until this ticket lands the TUI, which is how the product is actually used, still declines its mutations, so pausing before it would pause on a half-usable build.
The legacy-driver invariant holds until then: `~/.cargo/bin/mp` stays the preserved `mp-legacy` binary and the branch build is not installed over it.

## Residual risks carried in from the #0050 review

- Duplicate draft ids: two files carrying the same `id:` collapse to one `drafts` row.
  The reindex now picks a deterministic winner (newest file, ties by path) and reports both paths, in the log and on `mp`'s stderr, so the shadowed file is visible.
  The resolution semantics are still open: nothing renames, re-mints or refuses, and this ticket is where a TUI-side surfacing (a status line on the Drafts mailbox) would land if one is wanted.
- The one-second drafts poll refreshes the active account only, so a draft written into another configured account's directory is not seen until the user switches to it.
  Cheap to widen, deliberately not widened blind, because the scan cost then scales with the account count on every tick.
- The `.md`-selector defect found by the same review is fixed: the filesystem-path heuristic now runs only on unqualified input, so a Message-ID on a `.md` ccTLD and a draft id ending `.md` survive their own canonical form.
