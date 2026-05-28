//! [`Flex`] — taffy-driven flexbox container. Children declare a
//! `grow` weight; the container distributes available space
//! proportionally along [`FlexDirection`]. Per-frame taffy tree
//! (no caching in v1) keeps the implementation small.
//!
//! Taffy's `f32` layout is truncated at the taffy → widget boundary
//! via [`super::taffy_util::to_cell_rect`]; the [`Widget`] trait
//! stays cell-integer.

use crossterm::event::Event;
use ratatui::layout::Rect;
use taffy::prelude::*;
use taffy::style_helpers::{auto, length};

use super::taffy_util::to_cell_rect;
use super::{EventContext, EventOutcome, FocusId, Input, RenderContext, Widget, WidgetState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    Column,
}

pub struct FlexChild {
    pub widget: Box<dyn Widget>,
    pub grow: f32,
    pub shrink: f32,
}

impl FlexChild {
    pub fn new(widget: Box<dyn Widget>) -> Self {
        Self {
            widget,
            grow: 0.0,
            shrink: 1.0,
        }
    }

    pub fn with_grow(mut self, grow: f32) -> Self {
        self.grow = grow;
        self
    }

    pub fn with_shrink(mut self, shrink: f32) -> Self {
        self.shrink = shrink;
        self
    }
}

pub struct Flex {
    pub children: Vec<FlexChild>,
    pub direction: FlexDirection,
    pub gap: u16,
    state: WidgetState,
}

impl Flex {
    pub fn row(children: Vec<FlexChild>) -> Self {
        Self::new(FlexDirection::Row, children)
    }

    pub fn column(children: Vec<FlexChild>) -> Self {
        Self::new(FlexDirection::Column, children)
    }

    pub fn new(direction: FlexDirection, children: Vec<FlexChild>) -> Self {
        Self {
            children,
            direction,
            gap: 0,
            state: WidgetState::default(),
        }
    }

    pub fn with_gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    fn taffy_direction(&self) -> taffy::FlexDirection {
        match self.direction {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::Column => taffy::FlexDirection::Column,
        }
    }
}

impl Input for Flex {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        for child in &mut self.children {
            if matches!(
                child.widget.handle_event(ctx, event),
                EventOutcome::Consumed
            ) {
                return EventOutcome::Consumed;
            }
        }
        EventOutcome::Pass
    }
}

impl Widget for Flex {
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
        let (mut tree, child_nodes, root) = self.build_tree(width, None);
        if tree
            .compute_layout_with_measure(
                root,
                Size {
                    width: AvailableSpace::Definite(width as f32),
                    height: AvailableSpace::MaxContent,
                },
                measure_closure(&self.children),
            )
            .is_err()
        {
            return 0;
        }
        let _ = child_nodes; // children's layouts will be re-read in `layout`
        tree.layout(root)
            .map(|l| l.size.height.ceil() as u16)
            .unwrap_or(0)
    }

    fn layout(&mut self, area: Rect) {
        self.state.rect = area;
        if self.children.is_empty() {
            return;
        }
        let (mut tree, child_nodes, root) = self.build_tree(area.width, Some(area.height));
        if tree
            .compute_layout_with_measure(
                root,
                Size {
                    width: AvailableSpace::Definite(area.width as f32),
                    height: AvailableSpace::Definite(area.height as f32),
                },
                measure_closure(&self.children),
            )
            .is_err()
        {
            return;
        }
        for (i, child) in self.children.iter_mut().enumerate() {
            let layout = match tree.layout(child_nodes[i]) {
                Ok(l) => *l,
                Err(_) => continue,
            };
            let rect = to_cell_rect(&layout, area);
            child.widget.layout(rect);
        }
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        for child in &self.children {
            let rect = child.widget.state().rect;
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let mut child_ctx = ctx.with_area(rect);
            child.widget.render(&mut child_ctx);
        }
    }

    fn collect_focusable(&self, out: &mut Vec<FocusId>) {
        for child in &self.children {
            child.widget.collect_focusable(out);
        }
        if let Some(id) = self.state.focus_id {
            out.push(id);
        }
    }
}

impl Flex {
    /// Build a fresh taffy tree at the given width / optional fixed
    /// height. Returns `(tree, child_nodes, root)`; the order of
    /// `child_nodes` matches `self.children`.
    fn build_tree(
        &self,
        width: u16,
        height: Option<u16>,
    ) -> (TaffyTree<usize>, Vec<NodeId>, NodeId) {
        let mut tree: TaffyTree<usize> = TaffyTree::new();
        let mut child_nodes = Vec::with_capacity(self.children.len());
        for (idx, fc) in self.children.iter().enumerate() {
            let style = Style {
                flex_grow: fc.grow,
                flex_shrink: fc.shrink,
                ..Style::DEFAULT
            };
            // `expect` here is fine: new_leaf_with_context only errors
            // on internal taffy bugs we don't recover from gracefully.
            let leaf = tree
                .new_leaf_with_context(style, idx)
                .expect("taffy new_leaf_with_context");
            child_nodes.push(leaf);
        }
        let root_style = Style {
            display: Display::Flex,
            flex_direction: self.taffy_direction(),
            gap: Size {
                width: length(self.gap as f32),
                height: length(self.gap as f32),
            },
            size: Size {
                width: length(width as f32),
                height: match height {
                    Some(h) => length(h as f32),
                    None => auto(),
                },
            },
            ..Style::DEFAULT
        };
        let root = tree
            .new_with_children(root_style, &child_nodes)
            .expect("taffy new_with_children");
        (tree, child_nodes, root)
    }
}

#[expect(
    clippy::type_complexity,
    reason = "this is taffy's MeasureFunction shape; no simpler form available"
)]
fn measure_closure(
    children: &[FlexChild],
) -> impl FnMut(Size<Option<f32>>, Size<AvailableSpace>, NodeId, Option<&mut usize>, &Style) -> Size<f32>
+ '_ {
    move |known, available, _node_id, ctx, _style| {
        let idx = match ctx {
            Some(i) => *i,
            None => return Size::ZERO,
        };
        let width =
            known
                .width
                .map(|w| w.max(0.0) as u16)
                .unwrap_or_else(|| match available.width {
                    AvailableSpace::Definite(w) => w.max(0.0) as u16,
                    _ => 0,
                });
        let height = known
            .height
            .map(|h| h.max(0.0) as u16)
            .unwrap_or_else(|| children[idx].widget.measure(width));
        Size {
            width: width as f32,
            height: height as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
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
                for x in 0..ctx.area.width {
                    ctx.buf[(ctx.area.x + x, ctx.area.y + y)].set_symbol(&s);
                }
            }
        }
    }

    fn render(flex: &mut Flex, area: Rect) -> Buffer {
        flex.layout(area);
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
        flex.render(&mut ctx);
        buf
    }

    #[test]
    fn row_with_equal_grow_splits_evenly() {
        let mut flex = Flex::row(vec![
            FlexChild::new(Box::new(Leaf::new(1, 'a'))).with_grow(1.0),
            FlexChild::new(Box::new(Leaf::new(1, 'b'))).with_grow(1.0),
            FlexChild::new(Box::new(Leaf::new(1, 'c'))).with_grow(1.0),
        ]);
        flex.layout(Rect::new(0, 0, 9, 1));
        assert_eq!(flex.children[0].widget.state().rect.width, 3);
        assert_eq!(flex.children[1].widget.state().rect.width, 3);
        assert_eq!(flex.children[2].widget.state().rect.width, 3);
    }

    #[test]
    fn column_with_equal_grow_splits_evenly() {
        let mut flex = Flex::column(vec![
            FlexChild::new(Box::new(Leaf::new(1, 'a'))).with_grow(1.0),
            FlexChild::new(Box::new(Leaf::new(1, 'b'))).with_grow(1.0),
        ]);
        flex.layout(Rect::new(0, 0, 2, 6));
        assert_eq!(flex.children[0].widget.state().rect.height, 3);
        assert_eq!(flex.children[1].widget.state().rect.height, 3);
    }

    #[test]
    fn no_grow_uses_intrinsic_size() {
        // Children with grow=0 take their measured size; remainder
        // is empty space at the end of the row.
        let mut flex = Flex::row(vec![
            FlexChild::new(Box::new(Leaf::new(1, 'a'))),
            FlexChild::new(Box::new(Leaf::new(1, 'b'))),
        ]);
        // Leaves measure as 1 row tall. Their width comes from the
        // measure closure — given Auto width, they ask for 0 (no
        // intrinsic width), so taffy gives them whatever it computes.
        // Sanity: layout doesn't panic, total width is bounded by area.
        flex.layout(Rect::new(0, 0, 10, 1));
        let total: u16 = flex
            .children
            .iter()
            .map(|c| c.widget.state().rect.width)
            .sum();
        assert!(total <= 10);
    }

    #[test]
    fn render_paints_each_child() {
        let mut flex = Flex::row(vec![
            FlexChild::new(Box::new(Leaf::new(1, 'a'))).with_grow(1.0),
            FlexChild::new(Box::new(Leaf::new(1, 'b'))).with_grow(1.0),
        ]);
        let buf = render(&mut flex, Rect::new(0, 0, 4, 1));
        // Each child gets 2 cols. First two cells = 'a', next two = 'b'.
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "a");
        assert_eq!(buf[(2, 0)].symbol(), "b");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }

    #[test]
    fn empty_flex_no_panic() {
        let mut flex = Flex::row(vec![]);
        flex.layout(Rect::new(0, 0, 10, 1));
        assert_eq!(flex.measure(10), 0);
    }
}
