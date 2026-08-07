---
id: 0080
title: A draft the frontmatter parser skips vanishes silently from every listing
type: bug
priority: now
status: done
created: 2026-08-09
---

A draft whose YAML frontmatter will not parse disappears from the TUI Drafts list and `mp list` while the file sits on disk, with only a `log::warn` behind it.
The user experience is "my draft disappeared".

## Evidence (verified 2026-08-09)

- A user edits a draft's frontmatter in `$EDITOR` and mistypes an `attachments:` list item as `-"/path"` (no dash-space).
  The whole frontmatter block then parses to null, `store::drafts::scan` catches the parse error, logs `[drafts] skipping <path>: ...`, and drops the file: no index row, absent from the Drafts list and `mp list`.
- A second live case: a frontmatter that deserializes to null outright (`invalid type: null, expected struct EmailFrontmatter`), e.g. `perso/drafts/scarjo-take3.md`.
- The skip path in `scan` was `Err(e) => log::warn!(...)`, and `refresh_reporting` already surfaced its *other* invisible-draft case (id collisions) but not this one.
- The lessons-learned entry "A header value interpolated into a quoted YAML scalar has to escape both `\"` and `\\`" already named this failure mode ("the silent-skip mode #0064 named"), for a different trigger.

## Why it matters

Drafts are the one local-only thing in the product: agents write `.md` files and `$EDITOR` rewrites them behind the application's back, so a hand-edited YAML mistake is expected traffic, not a corruption.
A silent skip turns a one-character typo into a lost draft with no signpost to the file that needs fixing.

## Scope

1. `store::drafts`: `refresh_reporting` returns the skipped files (path + one-line parse error) beside the id collisions it already returned.
   The scan never fails the whole refresh over one bad file and never touches it.
2. TUI: a skipped draft shows in the Drafts list as an unopenable error row (warning glyph, filename as subject, theme `error` colour), with the parse error in the preview pane.
   `Enter`/`e` opens the raw file writable so the user can fix the YAML; `d` deletes the file by its path; send/approve/mark-draft decline cleanly (the row has no index id to resolve).
3. CLI: `mp list` prints a warning block after its listing naming each skipped file and its error; the exit code stays 0.

## Decisions

**The error row, not a status-line warning.**
The row puts the broken draft where the user expects the draft to be, in the list, at the top since it has no date to sort by.
The alternative, a persistent status-line warning, was the fallback if a pseudo-row fought the list architecture; it did not.
The skip is carried on a typed `EmailEntry::skip` field rather than overloading the `(msg: None, draft_id: None)` sentinel that already means "server-search hit", so no existing action mistakes a skip row for a search hit.

**Attachments null/empty stays as it is.**
A bare `attachments:` key already deserializes to `None` via the serde default, and `attachments: []` to `Some(vec![])`; both are tolerated with no auto-repair.
The live `-"/path"` trigger is a malformed *list*, which breaks the whole YAML block and is genuinely unparseable; auto-repairing it would be guessing at intent, which this ticket does not do.

## Acceptance criteria

- The scan returns the skipped files with a concise one-line error, and never fails the refresh.
- The Drafts list renders a skipped draft as an error row (pinned by a golden frame).
- `mp list` prints the warning block after the listing and exits 0.
- `Enter`/`e` on the error row opens the raw file; send/approve refuse cleanly; delete works by path.
