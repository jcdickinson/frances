//! [`Grid`] — taffy-driven CSS grid container. Children are placed
//! into rows × columns; track sizing is configured via
//! [`Grid::with_columns`] / [`Grid::with_rows`] using `1fr`-style
//! flex tracks or fixed lengths.
//!
//! Same model as [`super::flex::Flex`]: per-frame taffy tree,
//! `f32` truncated to integer cells by
//! [`super::taffy_util::to_cell_rect`].

use crossterm::event::Event;
use ratatui::layout::Rect;
use taffy::prelude::*;
use taffy::style_helpers::{flex, length};

use super::taffy_util::to_cell_rect;
use super::{EventContext, EventOutcome, FocusId, Input, RenderContext, Widget, WidgetState};

pub struct Grid {
    pub children: Vec<Box<dyn Widget>>,
    pub columns: Vec<GridTemplateComponent<String>>,
    pub rows: Vec<GridTemplateComponent<String>>,
    pub gap: u16,
    state: WidgetState,
}

impl Grid {
    pub fn new(children: Vec<Box<dyn Widget>>) -> Self {
        Self {
            children,
            columns: vec![],
            rows: vec![],
            gap: 0,
            state: WidgetState::default(),
        }
    }

    /// Convenience: an N-column grid with equal-sized tracks (`1fr` each).
    pub fn even_columns(children: Vec<Box<dyn Widget>>, count: u16) -> Self {
        let mut g = Self::new(children);
        g.columns = (0..count).map(|_| flex(1.0)).collect();
        g
    }

    pub fn with_columns(mut self, columns: Vec<GridTemplateComponent<String>>) -> Self {
        self.columns = columns;
        self
    }

    pub fn with_rows(mut self, rows: Vec<GridTemplateComponent<String>>) -> Self {
        self.rows = rows;
        self
    }

    pub fn with_gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }
}

impl Input for Grid {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        for child in &mut self.children {
            if matches!(child.handle_event(ctx, event), EventOutcome::Consumed) {
                return EventOutcome::Consumed;
            }
        }
        EventOutcome::Pass
    }
}

impl Widget for Grid {
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
        let (mut tree, _child_nodes, root) = self.build_tree(width, None);
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
            child.layout(rect);
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

impl Grid {
    fn build_tree(
        &self,
        width: u16,
        height: Option<u16>,
    ) -> (TaffyTree<usize>, Vec<NodeId>, NodeId) {
        let mut tree: TaffyTree<usize> = TaffyTree::new();
        let mut child_nodes = Vec::with_capacity(self.children.len());
        for (idx, _) in self.children.iter().enumerate() {
            let leaf = tree
                .new_leaf_with_context(Style::DEFAULT, idx)
                .expect("taffy new_leaf_with_context");
            child_nodes.push(leaf);
        }
        let mut root_style = Style {
            display: Display::Grid,
            grid_template_columns: self.columns.clone(),
            grid_template_rows: self.rows.clone(),
            gap: Size {
                width: length(self.gap as f32),
                height: length(self.gap as f32),
            },
            ..Style::DEFAULT
        };
        root_style.size = Size {
            width: length(width as f32),
            height: match height {
                Some(h) => length(h as f32),
                None => taffy::style_helpers::auto(),
            },
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
    children: &[Box<dyn Widget>],
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
            .unwrap_or_else(|| children[idx].measure(width));
        Size {
            width: width as f32,
            height: height as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn render(&self, _: &mut RenderContext<'_>) {}
    }

    #[test]
    fn two_by_two_even_grid_splits_into_quadrants() {
        let mut grid = Grid::even_columns(
            vec![
                Box::new(Leaf::new(1)),
                Box::new(Leaf::new(1)),
                Box::new(Leaf::new(1)),
                Box::new(Leaf::new(1)),
            ],
            2,
        )
        .with_rows(vec![flex(1.0), flex(1.0)]);
        grid.layout(Rect::new(0, 0, 8, 4));
        let rects: Vec<_> = grid.children.iter().map(|c| c.state().rect).collect();
        assert_eq!(rects[0], Rect::new(0, 0, 4, 2));
        assert_eq!(rects[1], Rect::new(4, 0, 4, 2));
        assert_eq!(rects[2], Rect::new(0, 2, 4, 2));
        assert_eq!(rects[3], Rect::new(4, 2, 4, 2));
    }

    #[test]
    fn empty_grid_no_panic() {
        let mut grid = Grid::new(vec![]);
        grid.layout(Rect::new(0, 0, 10, 5));
        assert_eq!(grid.measure(10), 0);
    }
}
