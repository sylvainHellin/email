---
id: 0093
title: Memoise the wrapped preview body and dirty-flag the redraw loop
type: perf
priority: now
status: done
created: 2026-08-14
---

## Resolution (2026-08-15)

Done. The wrapped/styled preview lines are memoised in `PreviewLinesCache`
(`src/tui/ui/preview.rs`), keyed by `(body epoch, pane width, inline-image
set)`; `render_body` rebuilds only on a key miss and renders the scrolled
window (`visible_slice`), so a scroll costs O(visible height). `PreviewBody`
gained a content `epoch` (`src/tui/app/types.rs`) that bumps only when the body
or its identity changes, giving the cache an O(1) hit test that stays stable
across the every-frame skip-draft/primed refills. `run_loop`
(`src/tui/mod.rs`) now carries a `dirty` flag and skips `terminal.draw` unless
input, resize, a watcher event, a background result, or a drafts change set it;
the spinner keeps its own slow tick while `bg_count > 0`. Theme is not a cache
key: it is fixed once at startup via a `OnceLock`, so no in-session change can
stale the baked-in colors.

The message body is re-parsed and re-wrapped on every frame (performance audit §b.2, confidence 0.85).
`ui::view` -> `preview::render_body` (`src/tui/ui/preview.rs:16`) calls `wrap_and_style_body(&body, inner_width)` (`preview.rs:55`) on every render, and `wrap_and_style_body` (`preview.rs:541`) walks the whole body, parses inline markdown, and word-wraps into `Vec<Line>`.
The text is memoised (`preview_body`) but the `Vec<Line>` product is not; it is rebuilt each pass.
For long emails this is the dominant per-keystroke cost in the preview and wasted CPU at idle.

This is compounded by an unconditional full redraw every loop iteration (performance audit §b.7, confidence 0.8).
`run_loop` calls `terminal.draw` at the top of every iteration regardless of whether state changed (`src/tui/mod.rs`), about four times a second at the 250 ms idle poll, so `ui::view` rebuilds all widget content each tick and multiplies the re-wrap above.

## Owner decision (2026-08-14)

Do both together: memoise the preview body wrap keyed by `(body, width)` and add a dirty-flag redraw loop.

## Scope

1. Memoise the wrapped and styled lines keyed by `(preview_body_key, inner_width)` next to `preview_body`, rebuilding only when the selection, body, or pane width changes, and render only the scrolled slice.
2. Track a dirty flag (state changed, resize, background result, spinner tick) and skip `terminal.draw` when nothing changed, keeping a slow tick for the spinner.

## Acceptance criteria

- Scrolling a long message rebuilds the styled body once per width change, not once per keystroke; preview scroll cost drops from O(body length) per frame to O(1).
- Idle CPU drops near zero: no full redraw fires when nothing changed, and the spinner still animates.
