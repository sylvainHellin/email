# Envelope dump allow-list

`mp dump-mailbox --json` is the parity oracle for the data-access-layer redesign ([#0049](tickets/0049-pre-nuke-oracle-capture.md) unit 0c).
It recorded one normalised record per message from the file-based stack, and [#0038](tickets/0038-read-path-to-db.md) flipped its source to the SQLite store.
The record shape, the field order and the sort contract are unchanged, so the flip is invisible for anything the two stacks can agree on.

This file lists every intended difference between the pre-nuke dump and the store-backed dump.
One line each, with the reason.
Anything not listed here is a regression, not a difference.

- `message_id` of mail with no `Message-ID:` header: was `null`, is now the synthetic `<sha256-<hex16>@local.invalid>` ingest assigns, because the store needs an identity for every row and the dump reports what the store holds rather than re-deriving the absence.
- `date_sort` of mail whose `Date:` header is missing or unparseable: was the date encoded in the file name, is now the empty string, because there is no file name any more; such mail sorts first instead of by its former filename date.
- `attachments[].size`: was `null` when the file named in the `attachments:` frontmatter list was not on disk (the normal case for outgoing mail, which recorded the source path it sent from), is now always the byte length of the stored blob.
- `from`, `to`, `cc`, `subject` that are stored as the empty string dump as `null`, because the store cannot distinguish an absent header from an empty one and the file build recorded `null` for the far commoner absent case.
- Drafts contribute no records until [#0050](tickets/0050-selector-contract-drafts-index.md) indexes them: they have no `messages` rows, so the `drafts` mailbox dumps empty, which is the documented stop-gate state of #0038 rather than a lost record.
- Non-UTF-8 messages no longer disappear: the file build dropped a `.md` file whose bytes were not valid UTF-8, and with no file to be unreadable the nearest case, an unreadable body blob, degrades to an empty body while the record still appears.
- The final sort tiebreaker is the `uid` instead of the file name, and since both are unique within a mailbox and neither is ever emitted, the order stays total and the output is unaffected.
