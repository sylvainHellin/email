---
id: 0070
title: Audit the website for file-era claims (per-mailbox local directories, and the rest)
type: chore
priority: later
status: done
created: 2026-08-06
closed: 2026-08-11
---

Deferred note from the fresh-context review of [#0057](0057-dead-file-era-code-deletion.md).
Effort: S.

The store cutover moved messages out of per-mailbox directories and into `store.db` plus a blob store, and #0057 removed the last of the directory talk from `mp config show`.
The published documentation still describes the old layout, so the site now tells a new user something the binary contradicts on first run.

## Evidence

- `website/src/pages/config.astro:182-193` states that each mailbox has a local directory derived from its role (`inbox/`, `archive/`, `sent/`, plus `drafts/`) under the account's data directory, and that an extra mailbox gets a subdirectory named after a slugified server name.
  None of those directories is created any more.
- The same paragraph is the one `mp config show` used to agree with, which is why it was believable; #0057 changed the binary and not the site.

## Scope

1. Rewrite the `[mailboxes]` section of `config.astro` against the store architecture: a mailbox is a role plus a server folder name, and what lands locally is rows in `store.db` and content-addressed blobs.
2. Audit the other pages of `website/src/pages/` for the same class of claim (a directory per mailbox, a `.md` file per message, editing a message file by hand) and align each with `mp config show`, `mp --help` and [docs/architecture.md](../architecture.md).
3. Where the file era genuinely still applies, say which part: drafts and attachments have real paths, and the distinction is the useful thing for a reader.

## Acceptance criteria

- No page claims a per-mailbox local directory.
- Every path shown on the site exists after a fresh `mp init` on this version.
- `cd website && pnpm build` clean.

## Resolution (2026-08-11)

The `[mailboxes]` prose had already been rewritten against the store; what still described the file era was the layout block above it and three other pages.

- `config.astro`: the data-directory tree now shows `store.sqlite3`, `blobs/`, `drafts/`, the per-message forward-attachment mirror and `contacts-cache.json`, i.e. exactly the three paths `mp config show` prints plus the two it does not. The Obsidian advice now points at `drafts/`, the half that is still files, instead of at a mail tree that no longer exists.
- `getting-started.astro`: first sync writes rows and blobs, not a `.md` plus a companion `.html` plus a `_attachments/` sibling.
- `faq.astro`: attachments and HTML answers rewritten (blobs beside the row; `o` opens through a private temp copy, `O` and `mp save` write where the user asks); "directories" in the multi-account answer became "local store".
- `draft-format.astro`: the attachments section was entirely about the receive path. It now says what is true of a draft (`attachments:` takes any readable path) and what a forward does (materialises the source's blobs into `accounts/<name>/attachments/<message-id>/` and writes those paths in), and points at `o` / `O` / `mp save` for reading one.

Verified the paths against `mp config show` under a scratch `MAILYPOPPINS_DATA_DIR`, and `pnpm build` is clean. The `Hero`/`Features` components keep the "plaintext files" framing, which is about drafts and is still true.
