---
id: TKT-0051
title: email status
type: feature
priority: now
status: done
created: 2026-08-05
---

Currently, we only have 'read/unread' as status for emails in the mailbox.
I would like to be able to track the email I read, answered to, and forwared.
We could keep the convention with the colored dot next to them - just add move options
TBD how to control this.
I would like the state to be automatically updated when email is forwarded or answered.
Would be great to have a command to see the list of related emails (similar group by conversation in other clients)
Precedence: unread, read, forwarded, answered.
Exact UI/UX TBD before implementation

## Decision (2026-08-07, before implementation)

The ticket predates #0064, which narrowed `EmailStatus` to the three draft states.
The axis below is therefore separate state and `EmailStatus` stays as #0064 left it.

- **Where it lives.** In `messages.flags`, the IMAP flag string the column already held, as the tokens `\Seen`, `\Answered` and the `$Forwarded` keyword (RFC 5788, what Thunderbird, Apple Mail and Dovecot write).
  Parsed into `types::MessageFlags`, a set of three booleans, because a message can be read, answered and forwarded at once; collapsing that into one value is a display decision, not a storage one.
- **No schema bump.** The column is already `TEXT` and sync pass 1 restates the flags of the whole window on every pass, so a store written by an older build heals itself on the next sync.
  A bump would have cost every user a full re-download for a column that was already there.
- **Server is truth, per backend.** IMAP states the whole flag set, so `ingest::apply_flags` writes what the server listed: a reply undone in another client comes back as undone here.
  Graph answers `isRead` and nothing else (answered lives in extended MAPI properties), so `ingest::apply_seen_flags` merges only that bit and cannot erase the other two.
  That asymmetry is deliberate and is the reason the two entry points exist.
- **Automatic update on send, not on draft creation.** A reply draft records `in_reply_to:` and a forward records `forwarded_from:` (the source's `Message-ID`); `send::mark_source_after_send` reads one of them after a successful submission, flags every local copy of the source, then issues `UID STORE +FLAGS` per mailbox.
  Marking at draft creation would claim an answer for a draft that is abandoned.
  The whole hook is best effort and never fails or retries a send that already succeeded; a `UID STORE` that did not land is corrected by the next sync, which restates the server's own answer.
- **Display precedence: unread > answered > forwarded > read.** The ticket line reads as an enumeration rather than an ordering, and new mail must never hide behind a history glyph.
  One marker column, one glyph: dot (unread, blue), reply arrow (answered, green), forward arrow (forwarded, teal), blank (read).
- **Manual control: deferred.** The axis is automatic (server sync plus our own sends); `m` stays read/unread only.
  A manual toggle can be added later if the automatic answer turns out to be wrong often enough to want overriding, which is the "TBD how to control this" answered rather than dropped.
- **Conversation grouping is out of scope.** "A command to see the list of related emails" is threading, which is [#0008](0008-threading-conversation-view.md); the `thread_id` column ingest already fills is what it will surface.

## Shipped

- `types::MessageFlags` plus the three token constants, with `parse` / `to_flag_string` / `with_seen`.
- IMAP pass 1 reads `\Answered` and `$Forwarded` next to `\Seen` (`imap_client/fetch.rs::flags_of`); `StoreFetch::known_flags` carries the set instead of a bool.
- `ingest::apply_flag` (merge-aware), `ingest::apply_flags` (IMAP, server states everything) and `ingest::apply_seen_flags` (Graph, seen only).
- `store::write::set_answered` / `set_forwarded`, and `set_read` now merges instead of overwriting, so marking a replied-to message unread no longer erases `\Answered`.
- `store::read::MessageRow::{flags, is_answered, is_forwarded}`; `mp dump-mailbox` reports `answered` and `forwarded` beside `seen`.
- `EmailFrontmatter::{in_reply_to, forwarded_from}`, written by the reply and forward builders, read once by the post-send hook.
- `imap_client::add_flag_on_server`, the `+FLAGS` write.
- TUI marker column with the precedence above (`tui/ui/list.rs::status_marker`).

## Verification

`cargo test` 919 green (900 before, +19 pinning the axis: flag round-trip, ingest, the IMAP-replaces / Graph-merges split, the read-toggle merge, the draft keys, the local half of the post-send hook, the dump tokens, the marker precedence).
Live: `mp sync -A assistant` and read-only `mp sync -A tum`, then `mp dump-mailbox` showing `answered` on messages replied to elsewhere.
