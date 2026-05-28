//! [`HStack`] — stateful horizontal container. Equal-fair width
//! split across children, with the remainder absorbed by the last
//! child. For weighted splits, use [`super::flex::Flex`].

use crossterm::event::Event;
use ratatui::layout::Rect;

use super::{EventContext, EventOutcome, FocusId, Input, RenderContext, Widget, WidgetState};

pub struct HStack {
    pub children: Vec<Box<dyn Widget>>,
    pub gap: u16,
    state: WidgetState,
}

impl HStack {
    pub fn new(children: Vec<Box<dyn Widget>>) -> Self {
        Self {
            children,
            gap: 0,
            state: WidgetState::default(),
        }
    }

    pub fn with_gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    fn child_widths(&self, total: u16) -> Vec<u16> {
        let n = self.children.len();
        if n == 0 {
            return vec![];
        }
        let gap_budget = self.gap.saturating_mul((n as u16).saturating_sub(1));
        let avail = total.saturating_sub(gap_budget);
        let each = avail / n as u16;
        let extra = avail % n as u16;
        let mut widths = vec![each; n];
        if let Some(last) = widths.last_mut() {
            *last = last.saturating_add(extra);
        }
        widths
    }
}

impl Input for HStack {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        for child in &mut self.children {
            if matches!(child.handle_event(ctx, event), EventOutcome::Consumed) {
                return EventOutcome::Consumed;
            }
        }
        EventOutcome::Pass
    }
}

impl Widget for HStack {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, width: u16) -> u16 {
        let widths = self.child_widths(width);
        self.children
            .iter()
            .zip(widths.iter())
            .map(|(c, &w)| c.measure(w))
            .max()
            .unwrap_or(0)
    }

    fn layout(&mut self, area: Rect) {
        self.state.rect = area;
        let widths = self.child_widths(area.width);
        let mut x = area.x;
        for (i, child) in self.children.iter_mut().enumerate() {
            if i > 0 {
                x = x.saturating_add(self.gap);
            }
            let w = widths.get(i).copied().unwrap_or(0);
            child.layout(Rect::new(x, area.y, w, area.height));
            x = x.saturating_add(w);
        }
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        for child in &self.children {
            let rect = child.state().rect;
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let mut child_ctx = ctx.with_area(rect);
            child.render(&mut child_ctx);
        }
    }

    fn collect_focusable(&self, out: &mut Vec<FocusId>) {
        for child in &self.children {
            child.collect_focusable(out);
        }
        if let Some(id) = self.state.focus_id {
            out.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use ratatui::buffer::Buffer;

    struct Leaf {
        marker: char,
        state: WidgetState,
    }

    impl Leaf {
        fn new(marker: char) -> Self {
            Self {
                marker,
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
            1
        }
        fn render(&self, ctx: &mut RenderContext<'_>) {
            let s = self.marker.to_string();
            for x in 0..ctx.area.width {
                ctx.buf[(ctx.area.x + x, ctx.area.y)].set_symbol(&s);
            }
        }
    }

    fn render(stack: &mut HStack, area: Rect) -> Buffer {
        stack.layout(area);
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
        stack.render(&mut ctx);
        buf
    }

    #[test]
    fn empty_stack_layout_no_panic() {
        let mut stack = HStack::new(vec![]);
        stack.layout(Rect::new(0, 0, 10, 1));
    }

    #[test]
    fn even_width_splits_evenly() {
        let mut stack = HStack::new(vec![
            Box::new(Leaf::new('a')),
            Box::new(Leaf::new('b')),
            Box::new(Leaf::new('c')),
        ]);
        stack.layout(Rect::new(0, 0, 9, 1));
        assert_eq!(stack.children[0].state().rect, Rect::new(0, 0, 3, 1));
        assert_eq!(stack.children[1].state().rect, Rect::new(3, 0, 3, 1));
        assert_eq!(stack.children[2].state().rect, Rect::new(6, 0, 3, 1));
    }

    #[test]
    fn remainder_lands_on_last_child() {
        let mut stack = HStack::new(vec![
            Box::new(Leaf::new('a')),
            Box::new(Leaf::new('b')),
            Box::new(Leaf::new('c')),
        ]);
        // 10 / 3 = 3, remainder 1 → last gets 4.
        stack.layout(Rect::new(0, 0, 10, 1));
        assert_eq!(stack.children[0].state().rect.width, 3);
        assert_eq!(stack.children[1].state().rect.width, 3);
        assert_eq!(stack.children[2].state().rect.width, 4);
    }

    #[test]
    fn gap_eats_from_width_budget() {
        let mut stack =
            HStack::new(vec![Box::new(Leaf::new('a')), Box::new(Leaf::new('b'))]).with_gap(2);
        // 10 - 2 (gap) = 8; split 4 + 4.
        stack.layout(Rect::new(0, 0, 10, 1));
        assert_eq!(stack.children[0].state().rect, Rect::new(0, 0, 4, 1));
        assert_eq!(stack.children[1].state().rect, Rect::new(6, 0, 4, 1));
    }

    #[test]
    fn render_paints_children_side_by_side() {
        let mut stack = HStack::new(vec![Box::new(Leaf::new('a')), Box::new(Leaf::new('b'))]);
        let buf = render(&mut stack, Rect::new(0, 0, 4, 1));
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "a");
        assert_eq!(buf[(2, 0)].symbol(), "b");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }
}
