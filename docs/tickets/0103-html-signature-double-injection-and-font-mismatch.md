---
id: 0103
title: HTML signature is spliced raw into the draft, double-injected, and renders at the wrong size
type: bug
priority: next
created: 2026-08-31
status: done
---

Two defects in the #0099 / #0102 signature path when the configured signature is an HTML file (the real-world case: a rich signature exported from another mail client).

## Problems

1. **Raw HTML in the editable body, then a double signature at send.** `config::resolve_signature_markdown` returned the signature source verbatim, so an HTML file landed as raw HTML in the Markdown draft body spliced at reply/forward/compose time (`draft.rs`). At send, `markdown_to_html` (`src/send.rs`) *also* injected `signature_html` at the `{{SIGNATURE}}` marker, so the HTML part carried the signature twice: once as Markdown-converted spliced HTML, once as the injected copy.
2. **Font mismatch.** The injected signature HTML kept its own inline styles while the typed body only got the head `<style>` wrapper, so signature and body rendered at different sizes. Compounded by clients (Gmail, Outlook) stripping head `<style>` blocks entirely, so the body fell back to the client default while any inline-styled fragment kept its size.

## Resolution (unified Markdown pipeline)

- `config::resolve_signature_markdown` now converts an HTML source to Markdown before returning it (`parse::looks_like_html` + `parse::html_to_markdown`, a narrow snippet converter that keeps links as `[text](url)` and line breaks as Markdown hard breaks). Markdown/plain sources pass through unchanged. This is the single conversion point, so every caller (draft creation, direct sends, invites) gets Markdown.
- The spliced Markdown signature is now the single source for both outgoing parts: the `text/plain` part carries it as Markdown text (`strip_signature_marker` still drops the `{{SIGNATURE}}` marker), and the HTML part gets it via the normal `markdown_to_html` of the whole body, so it inherits the body font-family/font-size and the mismatch disappears.
- The send-time `signature_html` injection at the `{{SIGNATURE}}` marker is removed. The marker is kept purely as the boundary where `markdown_to_html` splits to wrap the quoted section in a `<div>` (the anti-collapse behaviour); it no longer carries any signature text. For the send-time-only paths that have no editable draft (invites, direct sends), the Markdown signature is appended to the body Markdown before rendering, so it too goes through the shared converter.
- The outgoing HTML wrapper now sets the font-family/font-size both in the head `<style>` and as an inline style on a top-level content `<div>`, so body and signature stay consistent in clients that strip `<style>`.

No dual-version (separate html/plaintext) signature config was added; the unified Markdown pipeline replaces that idea.

## Tests

- `parse::looks_like_html` / `html_to_markdown`: HTML signature converts to Markdown, links preserved, no raw tags, entities decoded.
- `config::resolve_signature_converts_an_html_file_to_markdown` and `_inline_html_text`.
- `send::a_reply_carries_the_signature_once_in_each_part`: plain part has the signature once and no `{{SIGNATURE}}`; HTML part has it once as a rendered anchor.
- The `markdown_to_html` snapshots were updated for the new inline-styled content wrapper.
