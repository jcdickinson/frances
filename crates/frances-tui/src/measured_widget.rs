//! [`MeasuredWidget`] — a widget that knows its own height at a given
//! width.
//!
//! Intentionally distinct from [`crate::block::Block`]. `Block` is a
//! piece of scrollback content with the conventions that role drags
//! in: history promotion, spill into native scrollback, partial-area
//! top-clipping when the block straddles the top edge of the visible
//! window, [`crate::block::TruncatedBlock`] wrapping. A
//! `MeasuredWidget` is just a region of pixels the container reserves
//! and paints — used for the live UI pinned below the scrollback
//! (input box, status bar, whatever the caller wants).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub trait MeasuredWidget {
    /// Total rendered row count if wrapped at `width`. Must be
    /// deterministic for a given width — the container's layout
    /// decisions depend on it.
    fn measure(&self, width: u16) -> u16;

    /// Paint into `area`.
    fn render(&self, area: Rect, buf: &mut Buffer);
}

/// Adapter so a `ratatui::widgets::Paragraph` can be used directly.
/// Mostly useful for tests and the scratch binary; production widgets
/// generally implement the trait themselves.
impl MeasuredWidget for ratatui::widgets::Paragraph<'static> {
    fn measure(&self, width: u16) -> u16 {
        self.line_count(width) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        use ratatui::widgets::Widget;
        Widget::render(self.clone(), area, buf);
    }
}
