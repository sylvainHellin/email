---
id: 0106
title: Markdown-native signatures with hard-break rendering and TUI editing
type: feature
priority: now
status: done
created: 2026-08-16
---

## Problem

Since #0099 the signature is resolved to Markdown and appended to the body Markdown before rendering (`send::markdown_to_html`), instead of being injected as pre-styled HTML at send time.
That fixed the font/size divergence and the double-injection on replies, but CommonMark treats a lone `\n` as a soft break, so a line-oriented signature (name, title, address, links) collapses into one wrapped paragraph.
The signature renders visibly worse than the old HTML injection.

Owner decision, 2026-08-16: go Markdown-native rather than revert to HTML injection.
The signature is fully expressible in Markdown; the HTML-to-Markdown hop stays only as a paste-time convenience for an HTML source.

## Part 1: hard-break rendering

Add `to_hard_breaks` in `config.rs` and apply it inside `signature_source_to_markdown`, the single normalisation point every caller (draft creation, direct sends, invites) goes through.
Every non-blank line followed by another non-blank line gets the two trailing spaces that make a CommonMark hard break; blank lines stay paragraph breaks.
A line already ending in a space is left untouched, which preserves the RFC 3676 signature delimiter (two hyphens and a space).
Keep `parse::html_to_markdown` as a paste-time fallback for an HTML source, not the primary path.
Leave the `text/plain` part's trailing hard-break spaces in place: they are invisible to a plain reader and quoted-printable preserves them, whereas right-trimming every line would strip the delimiter's meaningful trailing space.

## Part 2: TUI signature editing

Add a `Signature` entry to `ComposeField` that shows the active signature's name; cycling it selects among the account's named signatures.
The edit action opens the signature's `path` file (or a temp file for inline `text`) in `$EDITOR` and reloads on exit, matching the body's `$EDITOR` pattern.
Reachable both in the compose wizard and on an existing draft in the list or preview.
Changing the signature on an existing draft re-splices the signature block in `body_markdown`, since the block is spliced at creation (`draft.rs`).

## Acceptance

A multi-line signature keeps its line breaks in the HTML part.
The delimiter's trailing space survives into the plain part.
The signature can be selected and edited from the TUI, on a new and on an existing draft.

## Done (2026-08-30)

Both parts shipped.
Part 1 was the hard-break normaliser in `signature_source_to_markdown` (`config.rs`).

Part 2 adds a `Signature` field to `ComposeField`, between Subject and Body, always in the navigation order (`src/tui/app/types.rs`).
The wizard carries `signature_name`, `signature_initial`, `available_signatures`, and a per-draft `signature_override` for an edited inline signature.
Cycling reuses Up/Down and Ctrl+n/Ctrl+p; `e` (or Ctrl+e) opens the selected signature in `$EDITOR` (`Action::ComposeEditSignature`, handled in `src/tui/actions.rs`).
A `path` signature is edited in place; an inline `text` signature is edited on a temp copy and applied to the draft only, never written to `config.toml`.

The spliced signature block is wrapped in `<!-- mp:sig-start -->` / `<!-- mp:sig-end -->` sentinel comments at draft creation (New, Reply, Forward), so `set_signature_block` (`draft.rs`) can find and replace it when the selection changes.
Both send paths (`markdown_to_html` and the plain part via `plain_text_body`) and the TUI preview strip the sentinels via `draft::strip_signature_sentinels`.
The active selection is recorded in the draft's `signature:` frontmatter field (absent means account default).
Existing drafts reach signature selection through the existing EditDraft flow (`ce` in Drafts): the new field rides along and a changed selection re-splices via `draft::rewrite_draft_signature`.
