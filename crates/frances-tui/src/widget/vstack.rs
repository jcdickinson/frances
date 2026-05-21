//! [`VStack`] — stateful vertical container with `Box<dyn Widget>`
//! children. Children are measured at the full container width and
//! stacked top-down; an optional `gap` of blank rows separates
//! siblings.

use crossterm::event::Event;
use ratatui::layout::Rect;

use super::{EventContext, EventOutcome, FocusId, Input, RenderContext, Widget, WidgetState};

pub struct VStack {
    pub children: Vec<Box<dyn Widget>>,
    pub gap: u16,
    state: WidgetState,
}

impl VStack {
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
}

impl Input for VStack {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        for child in &mut self.children {
            if matches!(child.handle_event(ctx, event), EventOutcome::Consumed) {
                return EventOutcome::Consumed;
            }
        }
        EventOutcome::Pass
    }
}

impl Widget for VStack {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, width: u16) -> u16 {
        if self.children.is_empty() {
            return 0;
        }
        let sum: u16 = self
            .children
            .iter()
            .map(|c| c.measure(width))
            .fold(0u16, u16::saturating_add);
        let gap_count = (self.children.len() as u16).saturating_sub(1);
        sum.saturating_add(self.gap.saturating_mul(gap_count))
    }

    fn layout(&mut self, area: Rect) {
        self.state.rect = area;
        let max_y = area.y.saturating_add(area.height);
        let mut y = area.y;
        for (i, child) in self.children.iter_mut().enumerate() {
            if i > 0 {
                y = y.saturating_add(self.gap);
            }
            if y >= max_y {
                // Out of room — give a zero-height rect for state
                // hygiene; render will skip it.
                child.layout(Rect::new(area.x, max_y, area.width, 0));
                continue;
            }
            let want = child.measure(area.width);
            let avail = max_y - y;
            let h = want.min(avail);
            child.layout(Rect::new(area.x, y, area.width, h));
            y = y.saturating_add(h);
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
    use crate::widget::{Focus, FocusManager, Theme};
    use ratatui::buffer::Buffer;

    struct Leaf {
        height: u16,
        marker: char,
        state: WidgetState,
    }

    impl Leaf {
        fn new(height: u16, marker: char) -> Self {
            Self {
                height,
                marker,
                state: WidgetState::default(),
            }
        }

        fn focusable(height: u16, marker: char, id: FocusId) -> Self {
            Self {
                height,
                marker,
                state: WidgetState {
                    focus_id: Some(id),
                    ..WidgetState::default()
                },
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
            let s = self.marker.to_string();
            for y in 0..ctx.area.height {
                ctx.buf[(ctx.area.x, ctx.area.y + y)].set_symbol(&s);
            }
        }
    }

    fn render(stack: &mut VStack, area: Rect) -> Buffer {
        stack.layout(area);
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
        stack.render(&mut ctx);
        buf
    }

    #[test]
    fn empty_measure_is_zero() {
        let stack = VStack::new(vec![]);
        assert_eq!(stack.measure(20), 0);
    }

    #[test]
    fn measure_sums_children() {
        let stack = VStack::new(vec![
            Box::new(Leaf::new(2, 'a')),
            Box::new(Leaf::new(3, 'b')),
        ]);
        assert_eq!(stack.measure(20), 5);
    }

    #[test]
    fn gap_contributes_n_minus_one_rows() {
        let stack = VStack::new(vec![
            Box::new(Leaf::new(1, 'a')),
            Box::new(Leaf::new(1, 'b')),
            Box::new(Leaf::new(1, 'c')),
        ])
        .with_gap(1);
        assert_eq!(stack.measure(20), 1 + 1 + 1 + 2); // 3 rows + 2 gaps
    }

    #[test]
    fn layout_places_children_top_down() {
        let mut stack = VStack::new(vec![
            Box::new(Leaf::new(2, 'a')),
            Box::new(Leaf::new(1, 'b')),
        ]);
        stack.layout(Rect::new(0, 5, 10, 5));
        assert_eq!(stack.children[0].state().rect, Rect::new(0, 5, 10, 2));
        assert_eq!(stack.children[1].state().rect, Rect::new(0, 7, 10, 1));
    }

    #[test]
    fn layout_respects_gap_between_children() {
        let mut stack = VStack::new(vec![
            Box::new(Leaf::new(1, 'a')),
            Box::new(Leaf::new(1, 'b')),
        ])
        .with_gap(2);
        stack.layout(Rect::new(0, 0, 10, 10));
        assert_eq!(stack.children[0].state().rect, Rect::new(0, 0, 10, 1));
        assert_eq!(stack.children[1].state().rect, Rect::new(0, 3, 10, 1));
    }

    #[test]
    fn layout_clips_overflow_to_zero_height() {
        let mut stack = VStack::new(vec![
            Box::new(Leaf::new(3, 'a')),
            Box::new(Leaf::new(3, 'b')),
            Box::new(Leaf::new(3, 'c')),
        ]);
        stack.layout(Rect::new(0, 0, 10, 4));
        assert_eq!(stack.children[0].state().rect, Rect::new(0, 0, 10, 3));
        // Second child gets only 1 row — what's left.
        assert_eq!(stack.children[1].state().rect, Rect::new(0, 3, 10, 1));
        // Third child gets zero-height clipped rect.
        assert_eq!(stack.children[2].state().rect.height, 0);
    }

    #[test]
    fn render_paints_each_child_in_its_band() {
        let mut stack = VStack::new(vec![
            Box::new(Leaf::new(2, 'a')),
            Box::new(Leaf::new(1, 'b')),
        ]);
        let buf = render(&mut stack, Rect::new(0, 0, 1, 3));
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(0, 1)].symbol(), "a");
        assert_eq!(buf[(0, 2)].symbol(), "b");
    }

    #[test]
    fn collect_focusable_recurses_in_order() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        let stack = VStack::new(vec![
            Box::new(Leaf::focusable(1, 'a', a)),
            Box::new(Leaf::new(1, '_')),
            Box::new(Leaf::focusable(1, 'b', b)),
        ]);
        let mut out = Vec::new();
        stack.collect_focusable(&mut out);
        assert_eq!(out, vec![a, b]);
    }
}
