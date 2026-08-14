---
id: 0091
title: HTML-to-text rendering through an external tool
type: feature
priority: later
status: open
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
