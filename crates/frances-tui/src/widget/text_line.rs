//! [`TextLine`] — single styled line. Pads to width on render so
//! the cell budget is fully consumed (avoids leftover cells from a
//! previous frame bleeding through); truncates on overflow. Replaces
//! the inline status-row painting that lived inside the old
//! `FooterBlock`.

use crossterm::event::Event;
use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;

use super::{EventContext, EventOutcome, Input, RenderContext, Widget, WidgetState};

pub struct TextLine {
    pub text: String,
    /// `None` → use [`super::Theme::dim`] at render time.
    pub style: Option<Style>,
    state: WidgetState,
}

impl TextLine {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: None,
            state: WidgetState::default(),
        }
    }

    pub fn with_style(mut self, style: Style) -> Self {
        self.style = Some(style);
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn set_style(&mut self, style: Option<Style>) {
        self.style = style;
    }
}

impl Input for TextLine {
    fn handle_event(&mut self, _: &mut EventContext<'_>, _: &Event) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Widget for TextLine {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, _: u16) -> u16 {
        1
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        if ctx.area.width == 0 || ctx.area.height == 0 {
            return;
        }
        let style = self.style.unwrap_or(ctx.theme.dim);
        let padded = pad_to_width(&self.text, ctx.area.width as usize);
        ctx.buf.set_string(ctx.area.x, ctx.area.y, &padded, style);
    }
}

fn pad_to_width(s: &str, target: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > target {
            break;
        }
        out.push(c);
        used += w;
    }
    if used < target {
        out.push_str(&" ".repeat(target - used));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn render_to_buf(line: &mut TextLine, area: Rect) -> Buffer {
        line.layout(area);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let frame_time = crate::widget::FixedFrameTime(0.0);
        let animation = crate::widget::AnimationGate::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame_time: &frame_time,
            animation: &animation,
        };
        line.render(&mut ctx);
        buf
    }

    fn row_string(buf: &Buffer, area: Rect, y: u16) -> String {
        (0..area.width)
            .map(|x| buf[(area.x + x, area.y + y)].symbol().to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn measure_is_one() {
        let line = TextLine::new("hi");
        assert_eq!(line.measure(80), 1);
    }

    #[test]
    fn pad_to_width_pads_short_string_with_spaces() {
        assert_eq!(pad_to_width("hi", 5), "hi   ");
    }

    #[test]
    fn pad_to_width_truncates_when_overflow() {
        assert_eq!(pad_to_width("hello world", 5), "hello");
    }

    #[test]
    fn pad_to_width_respects_wide_chars() {
        // CJK chars take 2 cells each. "中" + space = 3 cells.
        assert_eq!(pad_to_width("中", 3), "中 ");
    }

    #[test]
    fn render_paints_text_then_pads_with_spaces() {
        let mut line = TextLine::new("hi");
        let area = Rect::new(0, 0, 5, 1);
        let buf = render_to_buf(&mut line, area);
        assert_eq!(row_string(&buf, area, 0), "hi   ");
    }

    #[test]
    fn render_truncates_overflow() {
        let mut line = TextLine::new("hello world");
        let area = Rect::new(0, 0, 5, 1);
        let buf = render_to_buf(&mut line, area);
        assert_eq!(row_string(&buf, area, 0), "hello");
    }

    #[test]
    fn render_zero_width_is_no_op() {
        let mut line = TextLine::new("hi");
        let _ = render_to_buf(&mut line, Rect::new(0, 0, 0, 1));
    }
}
