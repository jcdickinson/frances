//! [`Border<W>`] — wraps a single child, eats 2 rows + 2 cols of
//! inner space, paints box-drawing characters via ratatui's
//! [`ratatui::widgets::Block`] + an optional title inset.

use crossterm::event::Event;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};

use super::{EventContext, EventOutcome, FocusId, Input, RenderContext, Widget, WidgetState};

pub struct Border<W: Widget> {
    pub child: W,
    pub title: Option<String>,
    state: WidgetState,
}

impl<W: Widget> Border<W> {
    pub fn new(child: W) -> Self {
        Self {
            child,
            title: None,
            state: WidgetState::default(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }
}

impl<W: Widget> Input for Border<W> {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        self.child.handle_event(ctx, event)
    }
}

impl<W: Widget> Widget for Border<W> {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, width: u16) -> u16 {
        let inner = width.saturating_sub(2);
        self.child.measure(inner).saturating_add(2)
    }

    fn layout(&mut self, area: Rect) {
        self.state.rect = area;
        self.child.layout(inner_rect(area));
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        if ctx.area.width == 0 || ctx.area.height == 0 {
            return;
        }
        let mut block = Block::default()
            .borders(Borders::ALL)
            .style(ctx.theme.border);
        if let Some(t) = &self.title {
            block = block.title(Line::styled(t.clone(), ctx.theme.border_title));
        }
        ratatui::widgets::Widget::render(block, ctx.area, &mut *ctx.buf);
        let child_rect = self.child.state().rect;
        if child_rect.width > 0 && child_rect.height > 0 {
            let mut child_ctx = ctx.with_area(child_rect);
            self.child.render(&mut child_ctx);
        }
    }

    fn collect_focusable(&self, out: &mut Vec<FocusId>) {
        self.child.collect_focusable(out);
        if let Some(id) = self.state.focus_id {
            out.push(id);
        }
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use ratatui::buffer::Buffer;

    struct Leaf {
        height: u16,
        state: WidgetState,
    }

    impl Leaf {
        fn new(height: u16) -> Self {
            Self {
                height,
                state: WidgetState::default(),
            }
        }
    }

    impl Input for Leaf {
        fn handle_event(&mut self, _: &mut EventContext<'_>, _: &Event) -> EventOutcome {
            EventOutcome::Pass
        }
    }

    impl Widget for Leaf {
        fn state(&self) -> &WidgetState {
            &self.state
        }
        fn state_mut(&mut self) -> &mut WidgetState {
            &mut self.state
        }
        fn measure(&self, _: u16) -> u16 {
            self.height
        }
        fn render(&self, ctx: &mut RenderContext<'_>) {
            for y in 0..ctx.area.height {
                ctx.buf[(ctx.area.x, ctx.area.y + y)].set_symbol("x");
            }
        }
    }

    fn render(widget: &mut Border<Leaf>, area: Rect) -> Buffer {
        widget.layout(area);
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
        widget.render(&mut ctx);
        buf
    }

    #[test]
    fn measure_adds_two() {
        let widget = Border::new(Leaf::new(3));
        assert_eq!(widget.measure(20), 5);
    }

    #[test]
    fn layout_insets_child_by_one() {
        let mut widget = Border::new(Leaf::new(3));
        widget.layout(Rect::new(0, 0, 10, 5));
        assert_eq!(widget.child.state().rect, Rect::new(1, 1, 8, 3));
    }

    #[test]
    fn render_paints_corners_and_child() {
        let mut widget = Border::new(Leaf::new(1));
        let buf = render(&mut widget, Rect::new(0, 0, 4, 3));
        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(3, 0)].symbol(), "┐");
        assert_eq!(buf[(0, 2)].symbol(), "└");
        assert_eq!(buf[(3, 2)].symbol(), "┘");
        // Child paints its 'x' at (1, 1) — the top-left inner cell.
        assert_eq!(buf[(1, 1)].symbol(), "x");
    }

    #[test]
    fn render_with_title_paints_title_on_top_row() {
        let mut widget = Border::new(Leaf::new(1)).with_title("hi");
        let buf = render(&mut widget, Rect::new(0, 0, 10, 3));
        assert_eq!(buf[(1, 0)].symbol(), "h");
        assert_eq!(buf[(2, 0)].symbol(), "i");
    }

    #[test]
    fn degenerate_zero_area_does_not_panic() {
        let mut widget = Border::new(Leaf::new(0));
        let _ = render(&mut widget, Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn collect_focusable_propagates_from_child() {
        use crate::widget::FocusManager;
        let mut mgr = FocusManager::new();
        let id = mgr.allocate();
        let mut leaf = Leaf::new(1);
        leaf.state.focus_id = Some(id);
        let widget = Border::new(leaf);
        let mut out = Vec::new();
        widget.collect_focusable(&mut out);
        assert_eq!(out, vec![id]);
    }
}
