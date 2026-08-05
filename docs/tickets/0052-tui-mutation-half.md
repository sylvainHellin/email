---
id: 0052
title: TUI mutation half on the store and the selector
type: refactor
priority: now
status: done
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
   Landed, unit B: `s` confirms, then submits the cursor draft through `send::send_durably` (or `send_durably_via` for Graph), which is the path `mp send <selector>` takes.
   The approved-status requirement is not restated in the TUI because it is not the CLI's either: `send::build_draft_message` enforces it, before the outbox row exists, and its refusal is the status line the user reads.
4. Approve and the batch approve.
   Landed, unit B.
5. Mark-draft and the batch mark-draft.
   Landed, unit B: both flips call the same `draft::mark_as_approved` / `draft::mark_as_draft` the CLI calls, so the legal transitions and the error text of an illegal one are one implementation, not two.
6. Edit recipients (the compose wizard's `EditDraft` mode) resolved through the drafts index rather than a cached path.
   Landed, unit A: the mode carries the draft's `id:` and resolves it on open and again on submit.
7. Open in `$EDITOR`, which is `mp edit <selector>`'s job done in-process.
   Landed, unit B, for a Drafts row, through the same suspend/edit/refresh seam the new drafts use.
   On a received row it declines permanently rather than with the #0052 line: the pre-nuke build handed `$EDITOR` the message's `.md`, that file no longer exists, and `mp edit` takes draft selectors only, so there is no CLI behaviour to port.
8. Attachment open and save, from the list and from the search-result overlay, sourced from `message_blobs`.
   Landed, unit C: `o` and `O` materialise the cursor row's blobs through `store::read::materialise_attachments` into `$TMPDIR/mailypoppins-<row id>`, which is where `mp open` puts them, and the picker and the directory picker above that address the files as they always did.
   The pre-nuke interaction is unchanged: no attachment says so, one skips the picker (`o` opens it, `O` goes straight to the directory picker), several open the picker, and a save collision keeps both copies under the `_1` rule `parse::save_attachment` has always applied.
   `mp open`'s own shortcut of opening every attachment at once stays CLI-only: a TUI that opened six windows on one keypress would be the surprising half of the two, and the residual risk recorded for it in #0050's review is a CLI risk, not a shared one.
9. Open in browser, from the list and from the search-result overlay: the rendered HTML comes from the html blob or the raw blob, not from a `.html` file beside a `.md`.
   Landed, unit C: `b` reads `store::read::load_html` (the html blob, or the html part of the raw message when there is no blob), writes it to `$TMPDIR/mailypoppins-<row id>/render/message.html` and hands the browser that file.
   The `render/` subdirectory keeps it out of reach of the row's own attachments, whose names are sanitised of path separators and so can never name a subdirectory.
   A sender who wrote no markup still gets the pre-nuke status line rather than an empty page.
10. Open event source, from the calendar view, through the invite's own row and ics blob.
    Landed, unit C: the agenda row's `MessageRef` resolves `store::read::load_invite_ics`, and `$EDITOR` is handed that `.ics` written to a temp file, where the file build handed it the invite's `.md`.
    Edits to that copy reach nothing, and there is no post-editor `refresh_calendar` any more for the same reason.
    The asymmetry with item 7 is deliberate: opening an event source is inspecting an artifact the message carries, which is worth doing on a copy, while opening a received message in `$EDITOR` is composition against a file that does not exist.
11. Search-result Open, Reply and Forward, which are the same three flows over a hit that resolved to a row.
    Reply and Forward landed, unit A, for both halves of a hit: one that resolved builds its source from the row, one that did not builds it from the content the overlay is already rendering (`draft::source_from_fetched`) rather than declining.
    The overlay's attachment and browser flows landed with unit C on the same two halves: a hit that resolved reads blobs, one that did not writes out the fetch's own attachment bytes and html part.
    Open declines permanently, like item 7 and for the same reason: nothing saves a hit to a file any more, `mp edit` takes draft selectors only, and an `$EDITOR` window over a temp copy whose edits go nowhere is a false affordance.
    The overlay already renders the headers and the body that window used to hold.

Housekeeping this ticket owns, so nothing survives by accident:

- Delete `draft::create_reply_draft`, `draft::create_forward_draft` and `draft::source_from_file`, the path-shaped test-only fixtures left over from the file build, and port the formatting tests in `tests/draft_integration.rs` onto `create_reply_draft_from` / `create_forward_draft_from` with a `SourceMessage`. The three file-attachment-hydration unit tests in `src/draft.rs` (#0006 lineage) die with them: the behaviour they cover does not exist post-nuke.
  Done, unit A.
  `parse::link_or_copy` went with them, its only caller having been `source_from_file`, and `main.rs`'s `source_from_row` plus `materialise_attachments` moved into the library (`draft::source_from_row`, `store::read::materialise_attachments`) so the TUI and the CLI build a draft's source through one function.
- Every remaining `needs_tui_mutation_half` decline disappears with the flow it guards; the helper itself goes when the last caller does.
  Done, unit C: the helper and its test are gone, and no status line in `src/tui/` cites #0052 any more.
- The multi-select set is keyed on `EntryKey` (a `MessageRef` or an indexed draft id) rather than on a `MessageRef` alone, landed in unit B.
  It had to be: `entry_from_draft` leaves `msg` empty, so a draft could not enter a `MessageRef`-keyed selection at all, and the batch approve and batch mark-draft of items 4 and 5 were reachable by keystroke and dead in fact.
  The received-mail batches (archive, delete, move, toggle-read) filter the set to its `MessageRef` half and the draft batches to its draft-id half; a mixed selection cannot arise, because one mailbox lists one kind of row and switching clears the set.

## Acceptance criteria

- No TUI action shows a decline status line for any flow listed above, with two named permanent carve-outs: `$EDITOR` on a received row (item 7) and Open on a server-search hit (item 11).
  Neither cites #0052, because neither is waiting for anything: `mp edit` takes draft selectors only, and the file both used to open stopped existing with #0037.
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

## Close-out

Landed in three units on `dal-greenfield`:

- unit A, `a366fde`, reply, forward, the compose wizard's Forward and EditDraft modes, and the adapter sunset.
- unit B, `9051253`, send, approve, mark-draft, their batch forms, `$EDITOR` on a draft, and the `EntryKey` rekeying of the multi-select set.
- unit C, this commit, attachments, the browser rendition, the event source, and the search overlay's half of all three.

The stop-gate is reached: #0038, #0050 and #0052 are done, so the TUI runs every one of its mutations off the store and the selector.
The legacy-driver invariant is released with it; installing the branch build over `~/.cargo/bin/mp` is now a decision rather than a violation.

The residual risks above stay open, none of them touched by this ticket.
One is added by unit C: the temp directory a row's attachments are materialised into is keyed by row id alone (`$TMPDIR/mailypoppins-<row id>`), which is `mp open`'s own name, so two accounts share a directory for the same id.
Nothing is served from it that was not just written, and only the paths written are opened or saved, so the collision is stale files left behind rather than wrong bytes handed out.
The permission half of that risk is closed by the follow-up pass below: the directory is created 0700 through `parse::materialisation_dir` and refused if the path is already something else, so the shared name leaks nothing and the stale-file collision is all that remains.

A second is named by the stop-gate review and left open deliberately: a forward whose source has attachments fails wholesale if any one of their blobs is missing, because `store::read::materialise_attachments` errors rather than skipping the file (`src/store/read.rs`).
A forward that silently dropped an attachment is the worse answer, so the refusal stands; it becomes reachable only once retention evicts attachment blobs, which nothing does yet, and the fix when it does is a partial forward that names what it could not attach.

## Stop-gate review follow-ups

Six findings from the review of unit C, none blocking, all fixed in one pass after the gate:

- The materialisation directories were created with the default mode at a predictable path, so on a shared host a directory (or symlink) an attacker created first received the message bytes.
  `parse::materialisation_dir` now creates them 0700, refuses a path that is not a real directory owned by this user, and tightens a loose mode left by an older build.
- `Action::Forward` was dead: `w` pushes `OpenComposeWizard(ComposeMode::Forward)` and nothing constructed the variant.
  Deleted, with its arm.
- The `mailypoppins-<row id>` name was spelled out in both `src/tui/actions.rs` and `src/main.rs`.
  Both now call `parse::materialisation_dir`, so the CLI/TUI parity this ticket asserts is carried by one function.
- `A` or `D` over a received-mail selection opened "Approve N drafts?" and then reported "Approved 0 drafts", because the batch takes the drafts half of the selection.
  The confirmation is now guarded on the selection holding at least one draft key, which is what the batch itself filters on, and says so on the status line instead.
- `materialise_attachments` joined the stored filename unsanitised, safe only because ingest sanitises upstream.
  It now sanitises at the write seam too, and two attachments sharing a name are disambiguated with the same `_1` rule as a save collision rather than one overwriting the other.
- The unit-C tests wrote into the real `$TMPDIR`, i.e. into `/tmp/mailypoppins-1`, the path a real `mp open` of row 1 uses.
  The fixture now moves `$TMPDIR` to a per-process directory under `$TMPDIR/mailypoppins-tests/`.
