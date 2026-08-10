//! Terminal graphics for the preview pane (#0010).
//!
//! The whole terminal-graphics surface of the application is this file. It
//! answers three questions and nothing else:
//!
//! - *Can this terminal draw pixels?* [`init`] asks the terminal itself, once,
//!   through `ratatui-image`'s capability query, and caches the answer for the
//!   process. Only a real graphics protocol counts: kitty, iTerm2 and sixel.
//!   `ratatui-image`'s halfblocks fallback is deliberately **not** taken --
//!   a terminal with no graphics keeps exactly the experience it had before
//!   this ticket, a `[image: name]` placeholder line, rather than a block of
//!   coloured cells nobody asked for.
//! - *How tall is an image?* [`rows_for`] turns intrinsic pixel dimensions and
//!   the queried cell size into a row count, so the renderer can reserve blank
//!   lines in the text flow and the image lands where the text says it is.
//! - *What does the renderer draw with?* [`PreviewImages`], the one-slot memo
//!   of the selected message's inline images, holding a decoded
//!   `StatefulProtocol` per image. Decoding happens once per cursor move, not
//!   once per frame, and `ratatui-image` re-encodes only when the target area
//!   changes.
//!
//! Nothing here runs headless: [`init`] is called from [`crate::tui::run`]
//! only, so every test, every golden frame and every non-TUI code path sees
//! no picker and takes the placeholder branch. That is what keeps
//! the golden frames deterministic -- no image cell can ever reach a
//! `TestBackend` buffer.

use std::io::Cursor;
use std::sync::OnceLock;

use image::ImageReader;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

use super::app::BodyKey;

/// Upper bound on the pixels of one inline image.
///
/// A decoded RGBA buffer is four bytes a pixel, so this caps one image at
/// ~160 MB of intermediate memory in the worst case and, more to the point,
/// caps the resize work the render pass pays for.
const MAX_PIXELS: u64 = 40_000_000;

/// Tallest an inline image may be drawn, in terminal rows.
///
/// A screenshot in a mail signature must not push the text out of the pane;
/// the image is scaled to fit, and what does not fit stays unrendered rather
/// than scrolling the body away.
pub(crate) const MAX_ROWS: u16 = 20;

/// Shortest an inline image may be drawn, in terminal rows.
pub(crate) const MIN_ROWS: u16 = 3;

/// The process-wide capability answer. `None` means "no graphics protocol";
/// `Some(picker)` carries the protocol and the cell size the query reported.
static PICKER: OnceLock<Option<Picker>> = OnceLock::new();

/// Ask the terminal what it can draw, once per process.
///
/// Must be called after entering the alternate screen and before the event
/// loop reads a key, because the query writes an escape sequence to stdout and
/// reads the reply off stdin: a concurrent reader would eat the reply. A
/// terminal that does not answer leaves `ratatui-image` at its halfblocks
/// default, which this function maps to "no graphics" for the reason in the
/// module docs.
pub(crate) fn init() {
    let resolved = match Picker::from_query_stdio() {
        Ok(picker) => match picker.protocol_type() {
            ProtocolType::Halfblocks => {
                log::info!("[images] no terminal graphics protocol; inline images stay textual");
                None
            }
            proto => {
                log::info!(
                    "[images] terminal graphics: {proto:?}, cell size {:?}",
                    picker.font_size()
                );
                Some(picker)
            }
        },
        Err(e) => {
            log::info!("[images] capability query failed ({e}); inline images stay textual");
            None
        }
    };
    let _ = PICKER.set(resolved);
}

/// The queried picker, or `None` when the terminal draws no pixels.
///
/// Also `None` before [`init`] runs, which is every non-TUI caller and every
/// test.
fn picker() -> Option<&'static Picker> {
    PICKER.get().and_then(|p| p.as_ref())
}

/// How many rows an image of `(width, height)` pixels needs when drawn
/// `cols` cells wide with a `(cell_w, cell_h)` pixel cell.
///
/// Aspect ratio is preserved and the image is never upscaled: a 16x16 favicon
/// stays a 16x16 favicon rather than being blown up to fill the pane. The
/// result is clamped into [`MIN_ROWS`]..=[`MAX_ROWS`], so a panorama is
/// letterboxed by `Resize::Fit` instead of eating the whole preview.
pub(crate) fn rows_for(
    (width, height): (u32, u32),
    cols: u16,
    (cell_w, cell_h): (u16, u16),
) -> u16 {
    if width == 0 || height == 0 || cols == 0 || cell_w == 0 || cell_h == 0 {
        return MIN_ROWS;
    }
    let avail_px = u64::from(cols) * u64::from(cell_w);
    let width_px = u64::from(width);
    let height_px = u64::from(height);
    // Scale down to the available width, never up.
    let drawn_h = if width_px > avail_px {
        height_px * avail_px / width_px
    } else {
        height_px
    };
    let rows = drawn_h.div_ceil(u64::from(cell_h)).max(1);
    (rows as u16).clamp(MIN_ROWS, MAX_ROWS)
}

/// The intrinsic pixel size of an encoded image, read from its header alone.
///
/// Header-only: this is the cheap half, paid even when the terminal cannot
/// draw the image, because the placeholder does not need the pixels and the
/// row reservation does not need them either.
pub(crate) fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let dims = reader.into_dimensions().ok()?;
    if u64::from(dims.0) * u64::from(dims.1) > MAX_PIXELS {
        return None;
    }
    Some(dims)
}

/// Decode `bytes` and hand them to the terminal's protocol.
///
/// `None` when the terminal draws no pixels, when the format is one the build
/// does not carry (the `image` dependency is cut down to png/jpeg/gif/webp/bmp
/// on purpose), or when the image is larger than [`MAX_PIXELS`].
pub(crate) fn protocol(bytes: &[u8]) -> Option<StatefulProtocol> {
    let picker = picker()?;
    dimensions(bytes)?;
    let decoded = image::load_from_memory(bytes)
        .map_err(|e| log::debug!("[images] undecodable inline image: {e}"))
        .ok()?;
    Some(picker.new_resize_protocol(decoded))
}

/// The cell size the capability query reported, or a 10x20 guess.
///
/// The guess is only ever used by the row arithmetic in a process where no
/// image will be drawn anyway, so its only job is to be a sane 1:2 ratio.
pub(crate) fn cell_size() -> (u16, u16) {
    picker().map_or((10, 20), |p| p.font_size())
}

/// One inline image of the previewed message.
///
/// `protocol` is `None` on a terminal without graphics (and in every test);
/// the renderer then draws the `[image: name]` placeholder line alone and
/// reserves no rows for it.
pub(crate) struct PreviewImage {
    /// The filename the part was sent under, shown in the placeholder line.
    pub(crate) name: String,
    /// Intrinsic pixel size, `None` when the header did not parse.
    pub(crate) dimensions: Option<(u32, u32)>,
    /// The terminal-protocol state, resized and re-encoded by the widget only
    /// when the target rect changes.
    pub(crate) protocol: Option<StatefulProtocol>,
}

impl PreviewImage {
    /// The rows this image needs inside a `cols`-wide pane, or `None` when it
    /// is not drawable here (no graphics protocol, or an unreadable header).
    pub(crate) fn rows(&self, cols: u16) -> Option<u16> {
        let dims = self.dimensions?;
        self.protocol.as_ref()?;
        Some(rows_for(dims, cols, cell_size()))
    }
}

/// One-slot memo of the inline images behind the preview pane.
///
/// The sibling of `PreviewBody` and `PreviewInvite`, keyed the same way and
/// refreshed in the same place: the images are needed for the message under
/// the cursor and no other. Moving the cursor costs one raw-blob read and one
/// decode per referenced image; a frame on an unchanged selection costs a key
/// comparison. A message with no attachments costs nothing at all, because the
/// list entry's `has_attachments` bit answers before any blob is touched.
#[derive(Default)]
pub(crate) struct PreviewImages {
    key: Option<BodyKey>,
    images: Vec<PreviewImage>,
}

impl PreviewImages {
    /// True when the memo already answers for `key`.
    pub(crate) fn holds(&self, key: &Option<BodyKey>) -> bool {
        &self.key == key
    }

    /// Park `images` as the inline images for `key`.
    pub(crate) fn fill(&mut self, key: Option<BodyKey>, images: Vec<PreviewImage>) {
        self.key = key;
        self.images = images;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &PreviewImage> {
        self.images.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut PreviewImage> {
        self.images.iter_mut()
    }
}

/// Whether `area` is entirely inside `bounds`.
///
/// An image is drawn only when the rows it needs are fully on screen: a kitty
/// or sixel image is painted by the terminal, not by the cell grid, so a
/// half-scrolled one would spill over the pane border instead of being clipped
/// the way text is.
pub(crate) fn fits_within(area: Rect, bounds: Rect) -> bool {
    area.width > 0
        && area.height > 0
        && area.x >= bounds.x
        && area.y >= bounds.y
        && area.right() <= bounds.right()
        && area.bottom() <= bounds.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headless_process_has_no_graphics() {
        // `init` is never called outside `tui::run`, so every test, every
        // golden frame and every CLI path takes the placeholder branch.
        assert!(picker().is_none());
        assert!(protocol(b"not an image").is_none());
    }

    #[test]
    fn rows_preserve_the_aspect_ratio_within_the_clamp() {
        // 400x200 px in 10x20 cells: 40 cols wide, 10 rows tall.
        assert_eq!(rows_for((400, 200), 40, (10, 20)), 10);
        // Narrower pane: scaled down to 20 cols => 200x100 px => 5 rows.
        assert_eq!(rows_for((400, 200), 20, (10, 20)), 5);
    }

    #[test]
    fn rows_never_upscale_a_small_image() {
        // A 16x16 favicon in a wide pane stays one row of pixels, clamped up
        // to the minimum so the placeholder line has something under it.
        assert_eq!(rows_for((16, 16), 80, (10, 20)), MIN_ROWS);
    }

    #[test]
    fn rows_clamp_a_tall_image() {
        assert_eq!(rows_for((100, 10_000), 40, (10, 20)), MAX_ROWS);
    }

    #[test]
    fn degenerate_inputs_get_the_minimum() {
        assert_eq!(rows_for((0, 0), 40, (10, 20)), MIN_ROWS);
        assert_eq!(rows_for((100, 100), 0, (10, 20)), MIN_ROWS);
        assert_eq!(rows_for((100, 100), 40, (0, 0)), MIN_ROWS);
    }

    #[test]
    fn dimensions_reads_a_png_header_and_rejects_junk() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .unwrap();
        assert_eq!(dimensions(&png), Some((1, 1)));
        assert_eq!(dimensions(b"nope"), None);
    }

    #[test]
    fn an_image_is_drawn_only_when_it_fits_the_pane() {
        let pane = Rect::new(2, 2, 40, 10);
        assert!(fits_within(Rect::new(2, 3, 20, 5), pane));
        // One row too tall: the bottom would land on the pane border.
        assert!(!fits_within(Rect::new(2, 3, 20, 10), pane));
        // Above the pane: a scrolled-past image.
        assert!(!fits_within(Rect::new(2, 1, 20, 3), pane));
        assert!(!fits_within(Rect::new(2, 3, 0, 3), pane));
    }
}
