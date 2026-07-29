---
id: TKT-0047
title: reconcile walks attachment .md files, so an attached REPLY can poison PARTSTATs
type: bug
priority: next
status: open
created: 2026-07-29
---

`reconcile::build_index` (`src/reconcile.rs:205-209`) walks the account root with an unbounded `WalkDir` and classifies every `*.md` it finds.
Inbound email attachments live under that same root: `parse.rs` writes them to `<mailbox>/<stem>_attachments/<name>.md` and mirrors them to `<account>/attachments/<message-id>/<name>.md`, and `sanitize_attachment_filename` preserves the `.md` extension.
Those files are sender-controlled content, not our mail.

A `.md` attached to an email can therefore carry frontmatter an attacker chooses.
`classify` requires `from`/`to`/`subject` plus an `event:` block, all trivial to supply, so a crafted attachment with `method: REPLY`, a real invite's UID and a high `sequence`/`dtstamp` wins the per-attendee reply tiebreak and `mp calendar rebuild` writes the forged `PARTSTAT` **into the real invite's frontmatter on disk**.
The live mailstore already holds 72 attachment `.md` files (none currently carrying an `event:` block, so there is no live symptom yet).

The Calendar view (#0034) had the same weakness in its own walk and fixed it locally: `is_attachment_path` in `src/tui/app/calendar_view.rs` skips any path with a component equal to `attachments` or ending in `_attachments`, and the invite's own `invite.ics` sidecar is still read, but only through `authoritative_ids`, keyed off a real email's path.
This ticket is the reconcile-side half of that fix, which is the more serious one because reconcile mutates disk while the calendar only renders.

Every other body-reading walk in the repo is already depth-limited (`max_depth(1)` at `draft.rs:28`, `contacts/extractor.rs:61`, `parse.rs:762`, `tui/app/types.rs:92,1168`), so reconcile and the calendar loader were the only two exposed.

## Acceptance criteria

- `build_index` skips attachment directories in both layouts, sharing one predicate with the calendar loader rather than duplicating the rule.
- A test seeds an attached `.md` with `method: REPLY`, a real invite's UID and a winning `(sequence, dtstamp)`, runs the reconcile, and asserts the real invite's attendee `PARTSTAT` is unchanged.
- The genuine `invite.ics` sidecar path keeps working (it lives inside an attachment dir by design).
