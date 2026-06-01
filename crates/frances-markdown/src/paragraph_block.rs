//! A leaf [`Block`] holding one paragraph's worth of styled spans.
//! `MarkdownSection::apply` emits one of these per `\n\n`-separated
//! paragraph in its accumulated text.
//!
//! Rendering goes through `ratatui::widgets::Paragraph` so word-wrap +
//! styled-span paint are out-of-the-box.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use frances_tui::block::{Block, BlockMeasureContext, BlockRenderContext, Sigil};
use frances_tui::widget::{EventContext, EventOutcome, Input};

use crate::inline::StyledSpan;

#[derive(Debug, Clone)]
pub struct ParagraphBlock {
    spans: Vec<StyledSpan>,
}

impl ParagraphBlock {
    pub fn new(spans: Vec<StyledSpan>) -> Self {
        Self { spans }
    }

    fn build_paragraph(&self) -> Paragraph<'static> {
        let rspans: Vec<Span<'static>> = self
            .spans
            .iter()
            .map(|s| Span::styled(s.text.clone(), s.style))
            .collect();
        let line = Line::from(rspans);
        Paragraph::new(Text::from(line))
            .wrap(Wrap { trim: false })
            .style(Style::default())
    }
}

impl Input for ParagraphBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for ParagraphBlock {
    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.build_paragraph().line_count(ctx.width) as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let para = self.build_paragraph();
        // ratatui's Paragraph has no native `src_y` straddle — emulate
        // by painting into a rect shifted up by `src_y` so the visible
        // window aligns. ratatui clips at the buffer edge.
        let shifted = Rect::new(
            ctx.area.x,
            ctx.area.y.saturating_sub(ctx.src_y),
            ctx.area.width,
            ctx.area.height.saturating_add(ctx.src_y),
        );
        Widget::render(para, shifted, ctx.buf);
        Sigil::blank()
    }
}
