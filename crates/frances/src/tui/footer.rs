//! `Footer` — `TextInput` (bordered textarea, one focusable child)
//! stacked on top of a one-row `TextLine` status row.

use crossterm::event::Event;
use frances_tui::{
    EventContext, EventOutcome, FocusId, FocusManager, Input, RenderContext, TextInput, TextLine,
    Widget, WidgetState,
};
use ratatui::layout::Rect;

/// Heights: `TextInput` is `TEXT_INPUT_HEIGHT = 3` (border + 1 row of
/// text), status row is `1`. Total: 4.
pub struct Footer {
    pub input: TextInput,
    pub status: TextLine,
    state: WidgetState,
}

impl Footer {
    pub fn new(focus: &mut FocusManager, placeholder: impl Into<String>) -> Self {
        Self {
            input: TextInput::new(focus, placeholder),
            status: TextLine::new("tokens: —"),
            state: WidgetState::default(),
        }
    }
}

impl Input for Footer {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        self.input.handle_event(ctx, event)
    }
}

impl Widget for Footer {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, width: u16) -> u16 {
        self.input.measure(width) + self.status.measure(width)
    }

    fn layout(&mut self, area: Rect) {
        self.state.rect = area;
        let input_h = self.input.measure(area.width).min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_h);
        let remaining = area.height.saturating_sub(input_h);
        let status_area = Rect::new(area.x, area.y + input_h, area.width, remaining);
        self.input.layout(input_area);
        self.status.layout(status_area);
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        if self.input.state().rect.height > 0 {
            let mut input_ctx = ctx.with_area(self.input.state().rect);
            self.input.render(&mut input_ctx);
        }
        if self.status.state().rect.height > 0 {
            let mut status_ctx = ctx.with_area(self.status.state().rect);
            self.status.render(&mut status_ctx);
        }
    }

    fn collect_focusable(&self, out: &mut Vec<FocusId>) {
        self.input.collect_focusable(out);
        self.status.collect_focusable(out);
    }
}
