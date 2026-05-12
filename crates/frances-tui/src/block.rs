//! [`Block`] — the unit of content for the scrollback container.
//!
//! A block measures itself at a given width and paints itself into a
//! `Buffer` region. Both history rows above the live area and the
//! footer slot use the same trait. The container knows about *the
//! shape* of a block (measurable, renderable) but not *the substance*
//! — a block can be a paragraph, a code listing, an input box, a
//! vstack of input+status, anything that fits the trait.
//!
//! The trait is ratatui-coupled: `Rect` and `Buffer` show up in the
//! signature. That's a deliberate trade — we get any ratatui widget
//! to participate cheaply, at the cost of needing to redo this layer
//! if we ever port to a non-ratatui frontend. The container logic
//! above (measure, decide visible, spill into native scrollback) is
//! renderer-agnostic and survives the swap.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub trait Block {
    /// Total rendered row count if wrapped at `width`. Must be
    /// deterministic for a given width — the container caches layout
    /// decisions on this.
    fn measure(&self, width: u16) -> u16;

    /// Paint into `area`. If `area.height < measure(area.width)`, the
    /// implementation should top-clip — render the top of its content
    /// up to `area.height` rows. The container only passes a partial
    /// area when a block straddles the top edge of the visible window.
    fn render(&self, area: Rect, buf: &mut Buffer);
}

/// Adapter so a `ratatui::widgets::Paragraph` can be used as a `Block`
/// directly. `Paragraph::render` consumes self upstream, so we clone —
/// Paragraph clones are cheap (owned Lines, no internal handles).
impl Block for ratatui::widgets::Paragraph<'static> {
    fn measure(&self, width: u16) -> u16 {
        // line_count requires the `unstable-rendered-line-info` cargo
        // feature, which is enabled in our Cargo.toml.
        self.line_count(width) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        use ratatui::widgets::Widget;
        Widget::render(self.clone(), area, buf);
    }
}
