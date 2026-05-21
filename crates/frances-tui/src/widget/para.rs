//! [`ParaWidget`] — newtype around `ratatui::widgets::Paragraph<'static>`
//! that owns a [`WidgetState`] and so satisfies [`Widget`]. The
//! scratchpad (`bin/container_scratch.rs`) and `ui.rs` tests use
//! this when they only need a "renders some text" footer.

use crossterm::event::Event;
use ratatui::widgets::Paragraph;

use super::{EventContext, EventOutcome, Input, RenderContext, Widget, WidgetState};

pub struct ParaWidget {
    pub paragraph: Paragraph<'static>,
    state: WidgetState,
}

impl ParaWidget {
    pub fn new(paragraph: Paragraph<'static>) -> Self {
        Self {
            paragraph,
            state: WidgetState::default(),
        }
    }
}

impl From<Paragraph<'static>> for ParaWidget {
    fn from(paragraph: Paragraph<'static>) -> Self {
        Self::new(paragraph)
    }
}

impl Input for ParaWidget {
    fn handle_event(&mut self, _: &mut EventContext<'_>, _: &Event) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Widget for ParaWidget {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, width: u16) -> u16 {
        // `line_count` requires the `unstable-rendered-line-info`
        // cargo feature, which is enabled in our workspace ratatui.
        self.paragraph.line_count(width) as u16
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        // Paragraph::render consumes self upstream; cloning is cheap
        // (owned Lines, no internal handles).
        ratatui::widgets::Widget::render(self.paragraph.clone(), ctx.area, ctx.buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    fn render_to_buf(widget: &ParaWidget, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame: 0,
        };
        widget.render(&mut ctx);
        buf
    }

    #[test]
    fn measure_matches_paragraph_line_count() {
        let para: Paragraph<'static> = Paragraph::new(vec![Line::raw("a"), Line::raw("b")]);
        let widget: ParaWidget = para.into();
        assert_eq!(widget.measure(80), 2);
    }

    #[test]
    fn wrapped_text_grows_measure() {
        let para: Paragraph<'static> =
            Paragraph::new(Line::raw("abcdefghij")).wrap(ratatui::widgets::Wrap { trim: false });
        let widget: ParaWidget = para.into();
        assert_eq!(widget.measure(4), 3, "10 cols / 4 = 3 rows");
    }

    #[test]
    fn render_paints_text_into_buffer() {
        let para: Paragraph<'static> = Paragraph::new(Line::raw("hello"));
        let widget: ParaWidget = para.into();
        let area = Rect::new(0, 0, 10, 1);
        let buf = render_to_buf(&widget, area);
        let row: String = (0..5)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap())
            .collect();
        assert_eq!(row, "hello");
    }

    #[test]
    fn handle_event_always_passes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut widget: ParaWidget = Paragraph::new(Line::raw("x")).into();
        let mut focus = Focus::new();
        let mut redraw = false;
        let mut ctx = EventContext {
            focus: &mut focus,
            redraw: &mut redraw,
        };
        let event = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(matches!(
            widget.handle_event(&mut ctx, &event),
            EventOutcome::Pass
        ));
    }
}
