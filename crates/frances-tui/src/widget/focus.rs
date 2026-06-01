//! Focus state — opaque [`FocusId`] keys allocated through a
//! [`FocusManager`] arena, with a per-frame [`Focus`] that tracks
//! the currently-focused id plus the tree-order list of every
//! focusable id (rebuilt each frame via [`Widget::collect_focusable`]).

use slotmap::{SlotMap, new_key_type};

use super::Widget;

new_key_type! {
    /// Opaque key for a focusable widget. Allocated via
    /// [`FocusManager::allocate`] at widget construction and
    /// stored on the widget's [`WidgetState`](super::WidgetState).
    pub struct FocusId;
}

/// Arena of allocated focus identities. Widgets allocate at
/// construction and release on drop.
#[derive(Default)]
pub struct FocusManager {
    arena: SlotMap<FocusId, ()>,
}

impl FocusManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allocate(&mut self) -> FocusId {
        self.arena.insert(())
    }

    pub fn release(&mut self, id: FocusId) {
        self.arena.remove(id);
    }
}

/// Per-frame focus state. `ordered` is the tree-order list of
/// focusable ids reachable from the root widget; `current` is the
/// id that receives events. Rebuilt at the top of each frame by
/// the app via [`Focus::refresh`].
#[derive(Default)]
pub struct Focus {
    ordered: Vec<FocusId>,
    current: Option<FocusId>,
}

impl Focus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> Option<FocusId> {
        self.current
    }

    pub fn is_focused(&self, id: FocusId) -> bool {
        self.current == Some(id)
    }

    /// Move focus to `id` iff it's currently registered. Calls
    /// targeting a stale id are silently dropped.
    pub fn set(&mut self, id: FocusId) {
        if self.ordered.contains(&id) {
            self.current = Some(id);
        }
    }

    /// Walk the root widget, collect every focusable id in
    /// pre-order, and validate `current` against the new ordering.
    /// If the previously-focused id is no longer present, snap to
    /// the first registered id (or `None` if the tree has no
    /// focusable widgets).
    pub fn refresh(&mut self, root: &dyn Widget) {
        self.ordered.clear();
        root.collect_focusable(&mut self.ordered);
        match self.current {
            Some(id) if self.ordered.contains(&id) => {}
            _ => self.current = self.ordered.first().copied(),
        }
    }

    pub fn move_next(&mut self) {
        self.rotate(1);
    }

    pub fn move_prev(&mut self) {
        self.rotate(-1);
    }

    fn rotate(&mut self, delta: isize) {
        if self.ordered.is_empty() {
            self.current = None;
            return;
        }
        let idx = self
            .current
            .and_then(|cur| self.ordered.iter().position(|&id| id == cur))
            .unwrap_or(0);
        let len = self.ordered.len() as isize;
        let next = ((idx as isize + delta).rem_euclid(len)) as usize;
        self.current = Some(self.ordered[next]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{EventContext, EventOutcome, Input, RenderContext, WidgetState};
    use crossterm::event::Event;
    use ratatui::layout::Rect;

    struct Leaf {
        state: WidgetState,
    }

    impl Leaf {
        fn focusable(id: FocusId) -> Self {
            Self {
                state: WidgetState {
                    focus_id: Some(id),
                    ..WidgetState::default()
                },
            }
        }

        fn inert() -> Self {
            Self {
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
        fn render(&self, _: &mut RenderContext<'_>) {}
    }

    struct Tree {
        state: WidgetState,
        children: Vec<Leaf>,
    }

    impl Input for Tree {
        fn handle_event(&mut self, _: &mut EventContext<'_>, _: &Event) -> EventOutcome {
            EventOutcome::Pass
        }
    }

    impl Widget for Tree {
        fn state(&self) -> &WidgetState {
            &self.state
        }
        fn state_mut(&mut self) -> &mut WidgetState {
            &mut self.state
        }
        fn measure(&self, _: u16) -> u16 {
            self.children.len() as u16
        }
        fn render(&self, _: &mut RenderContext<'_>) {}
        fn collect_focusable(&self, out: &mut Vec<FocusId>) {
            for child in &self.children {
                child.collect_focusable(out);
            }
        }
    }

    fn tree(children: Vec<Leaf>) -> Tree {
        Tree {
            state: WidgetState {
                rect: Rect::default(),
                focus_id: None,
            },
            children,
        }
    }

    #[test]
    fn refresh_collects_in_tree_order_and_picks_first() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        let c = mgr.allocate();
        let t = tree(vec![
            Leaf::focusable(a),
            Leaf::focusable(b),
            Leaf::focusable(c),
        ]);

        let mut focus = Focus::new();
        focus.refresh(&t);

        assert_eq!(focus.ordered, vec![a, b, c]);
        assert_eq!(focus.current(), Some(a));
    }

    #[test]
    fn inert_children_are_skipped() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let t = tree(vec![Leaf::inert(), Leaf::focusable(a), Leaf::inert()]);

        let mut focus = Focus::new();
        focus.refresh(&t);

        assert_eq!(focus.ordered, vec![a]);
    }

    #[test]
    fn move_next_wraps_through_registered_ids() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        let t = tree(vec![Leaf::focusable(a), Leaf::focusable(b)]);

        let mut focus = Focus::new();
        focus.refresh(&t);

        assert_eq!(focus.current(), Some(a));
        focus.move_next();
        assert_eq!(focus.current(), Some(b));
        focus.move_next();
        assert_eq!(focus.current(), Some(a));
    }

    #[test]
    fn move_prev_wraps_the_other_way() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        let t = tree(vec![Leaf::focusable(a), Leaf::focusable(b)]);

        let mut focus = Focus::new();
        focus.refresh(&t);

        focus.move_prev();
        assert_eq!(focus.current(), Some(b));
        focus.move_prev();
        assert_eq!(focus.current(), Some(a));
    }

    #[test]
    fn set_to_unregistered_id_is_a_noop() {
        let mut mgr = FocusManager::new();
        let registered = mgr.allocate();
        let unregistered = mgr.allocate();
        let t = tree(vec![Leaf::focusable(registered)]);

        let mut focus = Focus::new();
        focus.refresh(&t);
        focus.set(unregistered);

        assert_eq!(focus.current(), Some(registered));
    }

    #[test]
    fn refresh_snaps_to_first_when_current_disappears() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        let mut t = tree(vec![Leaf::focusable(a), Leaf::focusable(b)]);

        let mut focus = Focus::new();
        focus.refresh(&t);
        focus.set(b);
        assert_eq!(focus.current(), Some(b));

        t.children.pop();
        focus.refresh(&t);

        assert_eq!(focus.current(), Some(a));
    }

    #[test]
    fn empty_tree_yields_no_current() {
        let t = tree(vec![]);
        let mut focus = Focus::new();
        focus.refresh(&t);
        assert_eq!(focus.current(), None);
        focus.move_next();
        assert_eq!(focus.current(), None);
    }

    #[test]
    fn focus_manager_allocates_distinct_ids() {
        let mut mgr = FocusManager::new();
        let a = mgr.allocate();
        let b = mgr.allocate();
        assert_ne!(a, b);
    }
}
