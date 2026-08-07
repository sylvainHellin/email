---
id: 0075
title: No way to open a received message in $EDITOR since the store cutover
type: feature
priority: now
status: done
created: 2026-08-08
---

Reading a received message in `$EDITOR` was free when every message was a `.md` file on disk.
The store cutover (#0037) deleted the files and nothing replaced that affordance, so the capability was lost without ever being decided against.

## Evidence

- `Action::EditCurrent` declines outright on a received row: `"Open in $EDITOR needs a draft; received mail is a store row, not a file"` (`src/tui/actions.rs:764`, pre-fix).
  The comment beside it says the decline is permanent, "because nothing is coming that would make it work".
- The same decline is repeated for a server-search hit (`src/tui/actions.rs:2149`, pre-fix).
- The website still advertised the file-era behaviour: "Enter or e to open an email in your `$EDITOR`" (`website/src/pages/getting-started.astro:111`).
- Two neighbouring flows already materialise a rendition out of the store and hand it to a viewer: the browser `.html` (`html_temp_file`) and the invite `.ics` (`Action::OpenEventSource`).
  Only the message itself had no rendition.

## Why it matters

The preview pane is a pane, not a pager.
An editor gives search, folding, unlimited scrollback, block selection and yank into the system clipboard over the message body and its headers at once, which is what the pre-cutover build gave for free and what long threads, quoted chains and pasted logs actually need.

The store stays the source of truth.
Nothing about this asks for a writable message.

## Scope

1. A key on any message list (inbox, archive, sent, extra mailboxes) materialises the selected received message as Markdown with YAML frontmatter and opens it in `$EDITOR`.
2. The rendition is read-only: mode 0444 so the editor opens the buffer read-only, discarded when the editor exits, and edits are ignored by construction because nothing reads the file back.
3. It never lands in `drafts/` or anywhere the reconciler or the drafts index walks.
4. Reuse the existing suspend / launch / resume dance and the existing materialisation directory, both of which the invite-source flow already uses.
5. The frontmatter carries the second status axis (#TKT-0051): `read`, `answered`, `forwarded`.

Out of scope: attachments (the frontmatter lists their names, as the file era did, and `o` / `O` still materialise the bytes), and a CLI twin.

## Decisions

**Keybinding: `Enter` / `e`, the existing one.**
It is already "Open in editor" for drafts, it is already bound in the list and in the server-search overlay, and a received row is precisely where it used to work.
Nothing new to learn, nothing free to consume.
The description becomes "Open in editor (mail read-only)".

**Format: the file era's, which `crate::types::InboxFrontmatter` still deserialises.**
`from`, `to`, `cc`, `subject`, `date`, `message_id`, `attachments` and `read` are the keys the pre-cutover ingest wrote, in that order, emitted through `serde_yaml` exactly as it emitted them.
Three keys differ: `answered` and `forwarded` are new because the axis they belong to did not exist when the files did, and `mailbox` replaces the file era's `status:`, which was the directory a message sat in and is now the store's mailbox key.
A test round-trips the rendered frontmatter through `InboxFrontmatter` so the parity is pinned rather than asserted in prose.

**Location: `$TMPDIR/mailypoppins-<row id>/render/<subject slug>.md`.**
The same directory the browser rendition and the invite source already use, created 0700 and validated against a hostile pre-existing directory by `parse::materialisation_dir`.
`/tmp` being tmpfs is the right trade here: the file is a plain Markdown view a few kilobytes long, it is rebuilt from the store on every open, and it is deleted when the editor exits.
Nothing SQLite-adjacent goes there.
The `render/` subdirectory keeps it clear of the attachment names materialised beside it, which cannot name a subdirectory because ingest sanitises path separators out of them.

**Not done: a CLI twin.**
`mp show <selector>` does not exist; the store read surface is [#0062](0062-cli-store-read-surface.md), which owns `mp show` and `mp list-messages`.
The renderer added here (`store::read::render_markdown`) is the natural body of that command when #0062 is picked up.

## Known edges (post-ship review of `6bc486a`, recorded not fixed)

**An attachment named literally `render` denies the read-only open for that row.**
`parse::sanitize_attachment_filename` (`src/parse.rs:318-329`) replaces only `/`, `\` and NUL and strips control characters, so an attachment called `render` materialises as a regular file at `mailypoppins-<row id>/render`.
`render_temp_file`'s `create_dir_all` (`src/tui/actions.rs:80-83`) then fails and the open declines with "Open failed: creating ...".
In the reverse order, with the directory already there, `materialise_attachments` fails on `fs::write` into a directory and returns `Err` for the whole row, so `o` and `O` decline for every attachment of that message, not just the colliding one.

The collision is pre-existing: `render_temp_file` and the `render/` subdirectory predate this ticket, used by the browser rendition and the invite source.
It is denial only, with no escape from the 0700 per-row directory, which is why it is recorded here instead of fixed.
The Decisions line above ("cannot name a subdirectory because ingest sanitises path separators out of them") answers path traversal; it does not answer a plain file named `render`.

## Acceptance criteria

- `e` or `Enter` on a received row in any mailbox opens the message in `$EDITOR` as Markdown with frontmatter.
- The file is mode 0444 and gone after the editor exits.
- The frontmatter carries `read`, `answered` and `forwarded`.
- A Drafts row keeps opening its own file for editing, unchanged.
- Nothing is written under `drafts/` or anywhere `reconcile` walks.
- The help overlay and the website key table say what the key does.
