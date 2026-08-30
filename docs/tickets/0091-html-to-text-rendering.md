---
id: 0091
title: HTML-to-text rendering through an external tool
type: feature
priority: later
status: done
created: 2026-08-14
---

HTML-to-readable-text is the main comprehension gap of terminal mail (feature survey §c.10, audit synthesis §3).
Much real mail is HTML-only or HTML-dominant, and the current rendering is poor for it: `render_body` / `wrap_and_style_body` (`src/tui/ui/preview.rs`) parse inline markdown and word-wrap, which does not handle real-world HTML email well.
Today the escape hatch is `b`, opening the HTML rendition in a browser.

## Owner decision (2026-08-14)

An external tool dependency is worth it for readable HTML-to-text.
Evaluate w3m, lynx, and pandoc and pick one.
This closes the open question on whether HTML-to-text is worth an external dependency: yes.

## Scope

1. Evaluate w3m, lynx, and pandoc for HTML-to-text quality on representative real mail, and choose one.
2. Pipe the message HTML rendition through the chosen tool to produce readable plain text for the preview pane.
3. Degrade gracefully when the tool is absent: fall back to today's rendering rather than erroring or blanking the pane.
4. Keep the `b` open-in-browser escape hatch for mail the terminal cannot render well.

## Acceptance criteria

- An HTML-dominant message renders as readable text in the preview, materially better than today's markdown-wrap output.
- With the external tool not installed, the preview falls back to the current rendering and does not error.
- The chosen tool and the reasons it beat the other two are recorded (ticket note or `docs/lessons-learned.md`).

## Resolution

Delivered by rendering each message's own HTML through **html2text's rich
interface**, not through an external tool.

The owner note's "external tool is worth it, evaluate w3m / lynx / pandoc"
premise predates the discovery that `html2text` was **already a vendored
dependency**, used at ingest to flatten HTML to plain text (`src/parse.rs`
`html_to_plain`). The weak link was therefore not a missing renderer but a
lossy double conversion: HTML flattened to plain text at ingest, then re-parsed
as Markdown in the preview. The dependency survey
(`.agents/workflow/0091/dependency-choice.md`) weighed the three external tools
against the already-present crate; the owner approved **Option B** (html2text
rich, zero new dependencies) for zero-install portability, since an external
binary is a per-user runtime requirement Cargo cannot vendor and would not exist
on the static musl release target, whereas html2text ships inside the binary.

What changed:

- `render_html_body` / `style_for_annotations` in `src/tui/ui/preview.rs` render
  the HTML directly to styled `Line`s: html2text wraps to the pane width and
  returns per-span `RichAnnotation`s (strong, emphasis, links, code, CSS
  colours), mapped onto ratatui styles. Tables, lists, blockquotes and links
  come out structured instead of reconstructed.
- A `PreviewHtml` one-slot memo (`src/tui/app/types.rs`), refreshed by
  `App::refresh_preview_html` beside the body/invite/image memos, loads the
  message's HTML once per selection change via `store::read::load_html`.
- `render_body` picks the HTML path when the selected message carries an HTML
  part and falls back to `wrap_and_style_body` over the plain body otherwise
  (plain-only mail, drafts) or on a render error, so the pane never blanks or
  errors.
- The `b` / `tb` open-in-browser escape hatch is unchanged.

Acceptance criteria met: HTML-dominant mail renders as readable structured text
(unit tests in `preview.rs::html_body_tests`); a message with no HTML part, and
any the renderer refuses, fall back to today's output; the choice and its
rationale are recorded here and in the dependency-choice note.

Known cost (see follow-up): loading the HTML for an IMAP message re-parses its
raw RFC822 on each selection change, since there is no per-row `has_html` flag
to gate on. It is memoised (paid on cursor moves, never per frame) and mirrors
the existing inline-image and invite refreshers, but a `has_html` column would
let it skip plain-only mail the way inline images skip attachment-less rows.
