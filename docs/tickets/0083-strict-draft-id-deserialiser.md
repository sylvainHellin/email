---
id: 0083
title: Reject non-string `id:` scalars in draft frontmatter loudly instead of silently re-minting
type: bug
priority: later
status: done
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

## Resolution (2026-08-11)

Two guards, because one could not see all of it.

`EmailFrontmatter::id` deserialises through `types::strict_optional_id`: a YAML string (and an absent or bare key) is accepted, a number, boolean, list or mapping is an error naming the type. Nothing is coerced.
That alone misses the #0077 float shape: `gray_matter` coerces its `Pod` through `serde_json` on the way to the struct, and `8808e70039225152` is a YAML float whose value is infinity, which JSON flattens to `null` -- indistinguishable, by the time serde sees it, from a bare `id:`.
So `draft::reject_non_string_id` checks the `Pod` itself, before deserialisation, and produces the user-facing line: `frontmatter 'id:' is a number, not a string: quote it (id: "...") so the draft keeps its identity`.

The rejection routes through the existing #0080 skipped-draft path, so nothing new had to be built to tell the user: `mp list` prints the path and the reason, and the TUI Drafts list shows the broken file. The file is not rewritten, and no id is minted for it.

`set_draft_id` writes `id: "<id>"` (`yaml_dq_escape`), so the round trip is shape-stable regardless of content rather than resting on the minter starting ids with a letter. The `mp new` skeleton still writes the id bare: it is minted, so it cannot be number-shaped, and quoting it would churn every draft-creation golden for no behaviour change.

## Acceptance

A draft whose `id:` cannot be read as a string is never silently re-identified; the user is told which file and why. **Met** -- `a_number_shaped_id_is_rejected_loudly_and_never_re_minted` pins both #0077 shapes: two skipped drafts, zero indexed rows, both files byte-identical afterwards, each reported line naming the path and the reason. `a_quoted_number_shaped_id_is_a_perfectly_good_id` pins that the rejection is of the YAML shape, not of digits. Confirmed by hand against the installed binary in a scratch data dir.
