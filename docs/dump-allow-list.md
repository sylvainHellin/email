# Envelope dump allow-list

`mp dump-mailbox --json` is the parity oracle for the data-access-layer redesign ([#0049](tickets/0049-pre-nuke-oracle-capture.md) unit 0c).
It recorded one normalised record per message from the file-based stack, and [#0038](tickets/0038-read-path-to-db.md) flipped its source to the SQLite store.
The record shape, the field order and the sort contract are unchanged, so the flip is invisible for anything the two stacks can agree on.

This file lists every intended difference between the pre-nuke dump and the store-backed dump.
One line each, with the reason.
Anything not listed here is a regression, not a difference.

- `message_id` of mail with no `Message-ID:` header: was `null`, is now the synthetic `<sha256-<hex16>@local.invalid>` ingest assigns, because the store needs an identity for every row and the dump reports what the store holds rather than re-deriving the absence.
- `date_sort` of mail whose `Date:` header is missing or unparseable: was the date encoded in the file name, is now the empty string, because there is no file name any more; such mail sorts first instead of by its former filename date.
- `date_sort` of mail whose date came only from a `sent_at:` frontmatter field (legacy sent copies, which had no `date:`): was that timestamp, is now the `Date:` header of the server-fetched sent copy, because the store re-ingests sent mail from the server rather than keeping the local submission record.
  The two clocks differ by seconds in either direction (`Date:` is stamped when the message is built, `sent_at:` when submission finished); the live assistant-account check measured -24 s to +42 s over 62 sent copies.
  The empty string appears only when the fetched copy carries no parseable `Date:`, which the live check never observed.
- `attachments[].size`: was `null` when the file named in the `attachments:` frontmatter list was not on disk (the normal case for outgoing mail, which recorded the source path it sent from), is now always the byte length of the stored blob.
- `from`, `to`, `cc`, `subject` that are stored as the empty string dump as `null`, because the store cannot distinguish an absent header from an empty one and the file build recorded `null` for the far commoner absent case.
- `invite` of a message carrying any ics part: was `false` unless an `event:` block parsed out of the frontmatter (which required a parseable REQUEST-method `invite.ics`), is now `true` whenever an ics blob is stored, because presence is what the derive-on-read design keys on.
  This covers two sub-cases the live check measured on the tum account: unparseable ics (a garbage ics renders an empty card rather than hiding the invitation) and REPLY-method responses ("Accepted:", "Zugesagt:" mail, 29 of 31 flips), which legacy treated as plain mail but the store keeps as the input for own-PARTSTAT derivation.
- ics parts no longer appear in `attachments`: legacy listed `invite.ics` in the `attachments:` frontmatter, the store keeps the ics as its own invite blob kind, so the attachments array shrinks by exactly the ics entries.
- `attachments[].name` is the raw MIME part name: legacy recorded the on-disk file name after collision dedup (`image.png`, `image-1.png`), so two parts sharing one name now dump as two identical names.
  Collision handling moved from ingest to materialisation time (`_1` suffixing when opening or saving).
- `from`, `to`, `cc` of legacy-written sent copies: was the bare frontmatter value as typed (`user@host`), is now the address as the built RFC822 message renders it (`<user@host>`), because the store re-ingests the sent copy from the server instead of keeping the draft frontmatter.
- Drafts contribute no records, permanently: they are local `.md` files indexed in the `drafts` table by [#0050](tickets/0050-selector-contract-drafts-index.md) and have no `messages` rows, so the `drafts` mailbox dumps empty.
  The dump is an envelope-parity oracle for received mail against the pre-nuke stack, and drafts are deliberately outside that contract: `mp drafts` is where they are listed.
- A message the server lists in several mailboxes dumps one record per mailbox: Gmail cross-lists Sent and Inbox mail into All Mail (the archive mapping), the file tree kept one file in whichever mailbox synced it first, and the store records every listing.
  Mailbox record counts grow accordingly; the parity condition is that the pre-nuke record's mailbox is among the branch record's mailboxes for that `message_id`.
- The pre-nuke tree was a sync-limited window (legacy `mp sync` capped messages per mailbox), while the branch's first sync pulls the full server mailbox, so the branch dump is a superset.
  The parity direction is pre-nuke ⊆ branch; branch-only records are not differences.
- Non-UTF-8 messages no longer disappear: the file build dropped a `.md` file whose bytes were not valid UTF-8, and with no file to be unreadable the nearest case, an unreadable body blob, degrades to an empty body while the record still appears.
- The final sort tiebreaker is the `uid` instead of the file name, and since both are unique within a mailbox and neither is ever emitted, the order stays total and the output is unaffected.
- `thread` is a new field with no pre-nuke counterpart: it dumps `messages.thread_id`, the conversation key ingest assigns from the `In-Reply-To` / `References` chain ([#0008](tickets/0008-threading-conversation-view.md)), so a script can group related mail without re-parsing headers.
  It sits last in the record, after `invite`, so every earlier field keeps its position; the pre-nuke oracle simply had no such column.
