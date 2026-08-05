---
id: TKT-0047
title: reconcile walks attachment .md files, so an attached REPLY can poison PARTSTATs
type: bug
priority: next
status: open
created: 2026-07-29
---

Parked 2026-07-31 as an accepted risk, resolved by [#0040](0040-drop-file-layer-cutover.md).
The exposure the owner accepted: a sender-controlled `.md` attachment can carry a forged `method: REPLY` that `reconcile::build_index` classifies and writes into a real invite's `PARTSTAT`, and the live mailstore holds 72 attachment `.md` files today, none of them carrying an `event:` block.
No code is spent on it, because the data-access-layer rebuild deletes the walk it lives in: `src/reconcile.rs` moves onto store-backed sources in [#0038](0038-read-path-to-db.md) and there is no attachment `.md` on disk after the cutover.
See [data-access-layer](../plans/data-access-layer.md), decision F.
The analysis below stands as the record of the bug.

`reconcile::build_index` (`src/reconcile.rs:205-209`) walks the account root with an unbounded `WalkDir` and classifies every `*.md` it finds.
Inbound email attachments live under that same root: `parse.rs` writes them to `<mailbox>/<stem>_attachments/<name>.md` and mirrors them to `<account>/attachments/<message-id>/<name>.md`, and `sanitize_attachment_filename` preserves the `.md` extension.
Those files are sender-controlled content, not our mail.

A `.md` attached to an email can therefore carry frontmatter an attacker chooses.
`classify` requires `from`/`to`/`subject` plus an `event:` block, all trivial to supply, so a crafted attachment with `method: REPLY`, a real invite's UID and a high `sequence`/`dtstamp` wins the per-attendee reply tiebreak and `mp calendar rebuild` writes the forged `PARTSTAT` **into the real invite's frontmatter on disk**.
The live mailstore already holds 72 attachment `.md` files (none currently carrying an `event:` block, so there is no live symptom yet).

The Calendar view (#0034) had the same weakness in its own walk and fixed it locally: `is_attachment_path` in `src/tui/app/calendar_view.rs` skips any path with a component equal to `attachments` or ending in `_attachments`, and the invite's own `invite.ics` sidecar is still read, but only through `authoritative_ids`, keyed off a real email's path.
This ticket is the reconcile-side half of that fix, which is the more serious one because reconcile mutates disk while the calendar only renders.

One collision the shared predicate must account for: a server-side IMAP folder literally named "Attachments" slugifies (`config::slugify_mailbox_name`, `src/config.rs:661-674`) to `<account>/attachments`, the same path as the attachment mirror dir — a pre-existing on-disk collision, so sync would already interleave that folder's mail with mirrored attachments. Until the predicate can tell them apart, the accurate framing of the calendar loader's residual risk is "an IMAP folder named Attachments loses its invites", not "a hand-created custom mailbox".

Every other body-reading walk in the repo is already depth-limited (`max_depth(1)` at `draft.rs:28`, `contacts/extractor.rs:61`, `parse.rs:762`, `tui/app/types.rs:92,1168`), so reconcile and the calendar loader were the only two exposed.

## Acceptance criteria

- `build_index` skips attachment directories in both layouts, sharing one predicate with the calendar loader rather than duplicating the rule.
- A test seeds an attached `.md` with `method: REPLY`, a real invite's UID and a winning `(sequence, dtstamp)`, runs the reconcile, and asserts the real invite's attendee `PARTSTAT` is unchanged.
- The genuine `invite.ics` sidecar path keeps working (it lives inside an attachment dir by design).
- The "Attachments"-named-IMAP-folder collision is either handled or explicitly documented as unsupported.
