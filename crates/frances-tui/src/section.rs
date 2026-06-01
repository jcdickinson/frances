//! `Section` trait + [`SectionView`] — a section's blocks rendered as
//! one container entry.
//!
//! Sections are produced by workflows (via JS classes that map to
//! [`frances_models_tui::SectionKind`] variants) and dispatched by
//! the TUI's section dispatcher (in the binary's `frances::tui`
//! module). On each event the dispatcher re-applies the section, wraps
//! its current block list in a fresh [`SectionView`], and replaces the
//! section's single entry in the container. The `SectionView` owns the
//! blocks and paints the section's own streaming indicator while open —
//! streaming is a section concept, not a container one.
//!
//! See `docs/plan/section-and-markdown.md` for the full lifecycle.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::block::{Block, BlockMeasureContext, BlockRenderContext, Sigil};

/// Braille-dot frames cycled through by a live section's streaming
/// indicator. Single-cell glyphs, width 1 — they overlay cleanly on top
/// of any character. Advanced off [`BlockRenderContext::frame_time`].
const STREAMING_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub use frances_models_tui::SectionApply;

/// A live section — a state machine that consumes section events and
/// emits a fresh list of inner [`Block`]s on each `apply`. The
/// dispatcher (in `frances/src/ui.rs`) wraps that list in a
/// [`SectionView`] and replaces the section's single container entry.
/// Concrete impls live in `frances-markdown` (`MarkdownSection`) and
/// the binary's `tui::sections` module (`SingleBlockSection`).
///
/// `apply` returns an owned `Vec<Box<dyn Block>>` rather than a borrowed slice.
pub trait Section: 'static {
    /// Apply an event and return the section's full block list AFTER the event.
    /// Returns `Vec::new()` when the section has no renderable blocks yet.
    fn apply(&mut self, event: SectionApply<'_>) -> Vec<Box<dyn Block>>;

    /// Glyph painted in the left gutter at this section's first
    /// on-screen row. The dispatcher hands it to the [`SectionView`],
    /// which returns it from `render` so the container paints it into
    /// the gutter — one sigil for the whole section, not per inner block.
    fn sigil(&self) -> Sigil;
}

/// A section's inner blocks rendered as one container entry. Owns the section's
/// current blocks and sigil; renders them stacked (intra-section gap 0),
/// straddle-aware for the inspector. While `streaming`, paints its own
/// streaming indicator on its last content row.
///
/// A sealed section is just a `SectionView` with `streaming = false`;
/// the same type covers the live, safe, and committed lifetimes.
pub struct SectionView {
    blocks: Vec<Box<dyn Block>>,
    sigil: Sigil,
    streaming: bool,
}

impl SectionView {
    pub fn new(blocks: Vec<Box<dyn Block>>, sigil: Sigil, streaming: bool) -> Self {
        Self {
            blocks,
            sigil,
            streaming,
        }
    }

    /// Measure context for inner block `i`: selected iff the inspector's
    /// selected part is this child.
    fn child_measure_ctx<'a>(
        &self,
        ctx: &BlockMeasureContext<'a>,
        i: usize,
    ) -> BlockMeasureContext<'a> {
        BlockMeasureContext {
            width: ctx.width,
            selected: ctx.selected_part == Some(i),
            selected_part: None,
            theme: ctx.theme,
        }
    }

    /// Paint the streaming glyph just after the last non-blank cell of
    /// `row_y` within `ctx.area`. If the row runs to the right edge there's
    /// no trailing slot, so the glyph overwrites the final cell; an empty
    /// row puts it at the left so an open-but-blank section still reads as
    /// in flight.
    fn paint_streaming_indicator(&self, ctx: &mut BlockRenderContext<'_>, row_y: u16) {
        if ctx.area.width == 0 {
            return;
        }
        let right_edge = ctx.area.x + ctx.area.width - 1;
        let last_content_x = (ctx.area.x..=right_edge).rev().find(|&x| {
            let sym = ctx.buf[(x, row_y)].symbol();
            !sym.is_empty() && sym != " "
        });
        let glyph_x = match last_content_x {
            None => ctx.area.x,
            Some(x) if x < right_edge => x + 1,
            Some(_) => right_edge,
        };
        let len = STREAMING_FRAMES.len();
        let idx = (ctx.frame_time.get_frame() / 6.0).rem_euclid(len as f64) as usize;
        let cell = &mut ctx.buf[(glyph_x, row_y)];
        cell.set_symbol(STREAMING_FRAMES[idx % len]);
        cell.set_style(Style::default().fg(Color::Cyan));
    }
}

impl crate::widget::Input for SectionView {
    fn handle_event(
        &mut self,
        _ctx: &mut crate::widget::EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> crate::widget::EventOutcome {
        crate::widget::EventOutcome::Pass
    }
}

impl Block for SectionView {
    fn parts(&self) -> &[Box<dyn Block>] {
        &self.blocks
    }

    fn parts_mut(&mut self) -> &mut [Box<dyn Block>] {
        &mut self.blocks
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.blocks
            .iter()
            .enumerate()
            .map(|(i, b)| b.measure(&self.child_measure_ctx(ctx, i)))
            .fold(0u16, u16::saturating_add)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let src_window_top = ctx.src_y;
        let src_window_bot = ctx.src_y.saturating_add(ctx.area.height);
        let mut consumed: u16 = 0;
        let mut painted: u16 = 0;
        for (i, block) in self.blocks.iter().enumerate() {
            let child_selected = ctx.selected_part == Some(i);
            let h = block.measure(&BlockMeasureContext {
                width: ctx.area.width,
                selected: child_selected,
                selected_part: None,
                theme: ctx.theme,
            });
            if h == 0 {
                continue;
            }
            let block_top = consumed;
            let block_bot = consumed.saturating_add(h);
            consumed = block_bot;
            let overlap_top = block_top.max(src_window_top);
            let overlap_bot = block_bot.min(src_window_bot);
            if overlap_top >= overlap_bot {
                continue;
            }
            let block_src_y = overlap_top - block_top;
            let block_height = overlap_bot - overlap_top;
            let block_area = Rect::new(
                ctx.area.x,
                ctx.area.y + painted,
                ctx.area.width,
                block_height,
            );
            painted = painted.saturating_add(block_height);
            let mut child_ctx = BlockRenderContext {
                area: block_area,
                buf: &mut *ctx.buf,
                src_y: block_src_y,
                truncated: ctx.truncated,
                alt_view: ctx.alt_view,
                selected: child_selected,
                selected_part: None,
                theme: ctx.theme,
                frame_time: ctx.frame_time,
            };
            let _ = block.render(&mut child_ctx);
        }
        if self.streaming && painted > 0 {
            self.paint_streaming_indicator(ctx, ctx.area.y + painted - 1);
        }
        self.sigil.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::FixedFrameTime;
    use crate::widget::Theme;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Paragraph;

    fn para(text: &str) -> Box<dyn Block> {
        Box::new(Paragraph::new(text.to_owned())) as Box<dyn Block>
    }

    /// Render `view` into a fresh `width × height` buffer at `frame` and
    /// return the painted symbol grid as one string per row.
    fn render_rows(view: &SectionView, width: u16, height: u16, frame: f64) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let theme = Theme::default();
        let frame_time = FixedFrameTime(frame);
        let mut ctx = BlockRenderContext {
            area: Rect::new(0, 0, width, height),
            buf: &mut buf,
            src_y: 0,
            truncated: false,
            alt_view: false,
            selected: false,
            selected_part: None,
            theme: &theme,
            frame_time: &frame_time,
        };
        view.render(&mut ctx);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// A streaming section paints its braille glyph just after the last
    /// non-blank cell of its last content row.
    #[test]
    fn streaming_paints_indicator_after_last_content() {
        let view = SectionView::new(vec![para("hello")], Sigil::blank(), true);
        let rows = render_rows(&view, 20, 2, 0.0);
        assert_eq!(rows[0], "hello⠋");
    }

    /// A sealed (non-streaming) section renders verbatim — no glyph.
    #[test]
    fn sealed_section_paints_no_indicator() {
        let view = SectionView::new(vec![para("hello")], Sigil::blank(), false);
        let rows = render_rows(&view, 20, 2, 0.0);
        assert_eq!(rows[0], "hello");
    }

    /// The bug this whole change fixes: a multi-block (multi-paragraph)
    /// section paints the indicator ONLY on its last block's last row,
    /// never on the earlier, already-finalised paragraphs.
    #[test]
    fn indicator_only_on_last_block() {
        let view = SectionView::new(vec![para("aaa"), para("bbb")], Sigil::blank(), true);
        let rows = render_rows(&view, 20, 3, 0.0);
        assert_eq!(rows[0], "aaa", "first paragraph carries no indicator");
        assert_eq!(rows[1], "bbb⠋", "only the last paragraph streams");
    }

    /// Advancing the frame clock past a glyph boundary (6 frames at
    /// 60fps) flips the rendered glyph.
    #[test]
    fn frame_time_advances_glyph() {
        let view = SectionView::new(vec![para("hi")], Sigil::blank(), true);
        assert_eq!(render_rows(&view, 20, 2, 0.0)[0], "hi⠋");
        assert_eq!(render_rows(&view, 20, 2, 6.0)[0], "hi⠙");
    }
}
