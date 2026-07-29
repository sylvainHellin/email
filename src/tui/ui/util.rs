use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::super::app::Focus;
use super::super::theme;

pub(super) fn pane_border_style(current_focus: Focus, pane: Focus) -> Style {
    let focused = current_focus == pane || (current_focus == Focus::Search && pane == Focus::List);
    if focused {
        Style::default().fg(theme::active().border_focused)
    } else {
        Style::default().fg(theme::active().border)
    }
}

pub(super) fn hint_span(key: &str) -> Span<'_> {
    Span::styled(key, Style::default().fg(theme::active().accent))
}

pub(super) fn desc_span(desc: &str) -> Span<'_> {
    Span::styled(desc, Style::default().fg(theme::active().text_muted))
}

pub(super) fn truncate(s: &str, max_width: usize) -> String {
    if max_width <= 3 {
        return s.chars().take(max_width).collect();
    }
    let char_count = s.chars().count();
    if char_count <= max_width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_width - 1).collect();
        format!("{truncated}\u{2026}")
    }
}

/// Display width of a single char, treating control chars as zero-width so a
/// stray control byte can never make the window math overshoot.
fn char_display_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Total display width of a string (sum of per-char widths, control chars
/// counted as zero). Char/width-aware, never byte-based.
pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

/// Result of computing the visible slice of a single-line input field.
pub(super) struct VisibleWindow {
    /// The substring of the original text that fits in the viewport.
    pub text: String,
    /// The cursor's screen column *within the returned slice* (0-based,
    /// in display cells). Always `<= width`. Part of the helper contract for
    /// callers that place a real terminal cursor; the current append-only
    /// input renderers draw their own block glyph instead.
    #[allow(dead_code)]
    pub cursor_col: usize,
    /// Whether text was clipped on the left (i.e. there is hidden leading
    /// content and a left indicator may be warranted).
    pub clipped_left: bool,
    /// Whether text was clipped on the right (hidden trailing content).
    /// Unused by the append-only renderers (cursor is always at the tail, so
    /// nothing is ever hidden to the right) but kept for the general contract.
    #[allow(dead_code)]
    pub clipped_right: bool,
}

/// Compute a horizontally-scrolled window of a single-line input so the cursor
/// stays visible inside `width` display cells.
///
/// `cursor_char` is the cursor position as a *char index* into `text`
/// (`text.chars().count()` == cursor at end, the append-only case). The window
/// is chosen so the cursor is always within `[0, width]`, keeping some leading
/// context when scrolled. Slicing is char- and display-width-aware, never byte-
/// or naive-char-count-based, so umlauts/CJK never panic or misalign.
pub(super) fn visible_window(text: &str, cursor_char: usize, width: usize) -> VisibleWindow {
    let chars: Vec<char> = text.chars().collect();
    let cursor_char = cursor_char.min(chars.len());

    if width == 0 {
        return VisibleWindow {
            text: String::new(),
            cursor_col: 0,
            clipped_left: false,
            clipped_right: !chars.is_empty(),
        };
    }

    // Prefix display width up to (and excluding) each char index.
    let mut prefix_width = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0usize;
    prefix_width.push(0usize);
    for &c in &chars {
        acc += char_display_width(c);
        prefix_width.push(acc);
    }
    let total_width = acc;
    let cursor_disp = prefix_width[cursor_char];

    // If everything fits, no scrolling needed.
    if total_width <= width {
        return VisibleWindow {
            text: text.to_string(),
            cursor_col: cursor_disp,
            clipped_left: false,
            clipped_right: false,
        };
    }

    // Choose the first visible char so the cursor sits at the right edge with
    // a small margin of trailing context, but never scroll past the start.
    // We want: cursor_disp - start_disp <= width  (cursor visible), and prefer
    // to keep the cursor near the right edge when scrolled.
    let mut start = 0usize;
    // Advance `start` until the cursor fits within `width` cells from it.
    while start < cursor_char && cursor_disp - prefix_width[start] > width {
        start += 1;
    }
    let clipped_left = start > 0;

    // Fill forward from `start` up to `width` display cells.
    let mut end = start;
    while end < chars.len() && prefix_width[end + 1] - prefix_width[start] <= width {
        end += 1;
    }
    let clipped_right = end < chars.len();

    let slice: String = chars[start..end].iter().collect();
    let cursor_col = cursor_disp - prefix_width[start];

    VisibleWindow {
        text: slice,
        cursor_col,
        clipped_left,
        clipped_right,
    }
}

/// Render an append-only single-line input value scrolled so its end (where
/// the cursor sits) stays visible within `width` display cells. When the text
/// is scrolled off the left, the leading visible cell is replaced with an
/// ellipsis so the user knows content is hidden. Char/width-aware.
pub(super) fn scrolled_input_value(text: &str, width: usize) -> String {
    let cursor_char = text.chars().count();
    let window = visible_window(text, cursor_char, width);
    if window.clipped_left {
        let mut it = window.text.chars();
        it.next();
        format!("\u{2026}{}", it.as_str())
    } else {
        window.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ascii_fits_unchanged() {
        let w = visible_window("hello", 5, 20);
        assert_eq!(w.text, "hello");
        assert_eq!(w.cursor_col, 5);
        assert!(!w.clipped_left);
        assert!(!w.clipped_right);
    }

    #[test]
    fn long_ascii_scrolls_to_keep_end_visible() {
        let text = "abcdefghijklmnopqrstuvwxyz"; // 26 wide
        let w = visible_window(text, text.chars().count(), 10);
        // Cursor (at end) must be visible within the 10-cell window.
        assert!(w.cursor_col <= 10);
        // The window shows the tail, so it must end with the last char.
        assert!(w.text.ends_with('z'));
        assert!(w.clipped_left);
        assert!(!w.clipped_right);
        // Rendered slice never exceeds the width in display cells.
        assert!(w.text.chars().map(char_display_width).sum::<usize>() <= 10);
    }

    #[test]
    fn cursor_mid_text_stays_visible() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        // Cursor at char 13 (middle). Window width 10.
        let w = visible_window(text, 13, 10);
        assert!(w.cursor_col <= 10);
        assert!(w.text.chars().map(char_display_width).sum::<usize>() <= 10);
    }

    #[test]
    fn umlauts_no_panic_correct_window() {
        // Umlauts are width-1 but multi-byte; naive byte slicing would panic.
        let text = "Zürich Straße Müller Köln Österreich";
        let w = visible_window(text, text.chars().count(), 12);
        assert!(w.cursor_col <= 12);
        assert!(w.text.chars().map(char_display_width).sum::<usize>() <= 12);
        assert!(w.text.ends_with('h')); // ...Österreich
    }

    #[test]
    fn cjk_wide_chars_no_panic_correct_window() {
        // CJK chars are width-2 each.
        let text = "日本語のテキスト入力"; // 10 wide-2 chars = 20 cells
        let w = visible_window(text, text.chars().count(), 9);
        // Window must not exceed 9 cells even though chars are width-2.
        let disp: usize = w.text.chars().map(char_display_width).sum();
        assert!(disp <= 9, "display width {disp} exceeded 9");
        assert!(w.cursor_col <= 9);
        assert!(w.clipped_left);
    }

    #[test]
    fn empty_text() {
        let w = visible_window("", 0, 10);
        assert_eq!(w.text, "");
        assert_eq!(w.cursor_col, 0);
        assert!(!w.clipped_left);
        assert!(!w.clipped_right);
    }

    #[test]
    fn zero_width_does_not_panic() {
        let w = visible_window("abc", 3, 0);
        assert_eq!(w.text, "");
        assert_eq!(w.cursor_col, 0);
    }
}
