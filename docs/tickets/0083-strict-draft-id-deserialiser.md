---
id: 0083
title: Reject non-string `id:` scalars in draft frontmatter loudly instead of silently re-minting
type: bug
priority: later
status: open
created: 2026-08-10
---

Follow-up from #0077's root cause (see its close-out and `docs/lessons-learned.md`).

Minted ids now start with a letter, so no id we write can be misread as a YAML number.
But `set_draft_id` still writes the id unquoted, and `EmailFrontmatter::id` is `Option<String>`, so a hand-written numeric-looking id (from `$EDITOR` or an agent, e.g. `id: 123e456`) deserialises to `None` and the next refresh silently mints a replacement: the draft's identity changes under every selector and index row, with no error anywhere.

## Scope

1. A lenient-on-read, strict-on-nonsense deserialiser for `id:`: accept a YAML string as today, coerce nothing, and surface a non-string scalar as a loud per-draft error (skip the draft with a printed warning naming the file) instead of `None`.
2. Quote the id in `set_draft_id`'s writer so round-trips are shape-stable regardless of content.
3. Tests: the two #0077 failure shapes (`8808e70039225152` float, `1234567890123456` integer) hand-written into a draft file must produce the warning path, not a re-mint.

## Acceptance

A draft whose `id:` cannot be read as a string is never silently re-identified; the user is told which file and why.
