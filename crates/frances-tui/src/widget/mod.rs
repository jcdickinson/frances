//! Widget framework — [`Input`] + [`Widget`] trait split, per-frame
//! contexts ([`RenderContext`], [`EventContext`]), a slotmap-keyed
//! [`Focus`] arena, and (in later commits) container + primitive
//! widgets.
//!
//! ## The trait split
//!
//! [`Input`] is the event-handling half; [`Widget`] adds 1D
//! measurement, layout, and rendering on top. The split exists so
//! Phase C blocks can accept events (the alt-view inspector wants
//! per-block hscroll/vscroll) without becoming widgets in their own
//! right.
//!
//! ## Measurement is 1D
//!
//! Widgets answer "given this width, how tall are you?" — same shape
//! as [`crate::block::Block`]. Containers that need real 2D layout
//! ([`flex`], [`grid`]) use [`taffy`](https://docs.rs/taffy) under
//! the hood; the trait surface stays cell-integer.
//!
//! ## Layout / render split
//!
//! [`Widget::layout`] stashes the widget's resolved [`Rect`] in its
//! [`WidgetState`] (and recurses into children for containers);
//! [`Widget::render`] paints into the rect via the
//! [`RenderContext::area`] passed by the parent. Storing the rect
//! on the widget keeps render closures small and lets parent
//! containers reach into a child's resolved rect when needed.

pub mod context;
pub mod focus;
pub mod input;
pub mod theme;

pub use context::{EventContext, RenderContext};
pub use focus::{Focus, FocusId, FocusManager};
pub use input::{EventOutcome, Input};
pub use theme::Theme;

use ratatui::layout::Rect;

/// State every widget embeds. Holds the resolved [`Rect`] from the
/// most recent [`Widget::layout`] call (used by render + by parent
/// containers needing to position content relative to a child) plus
/// the widget's optional [`FocusId`] (set iff the widget accepts
/// focus).
#[derive(Default, Clone)]
pub struct WidgetState {
    pub rect: Rect,
    pub focus_id: Option<FocusId>,
}

pub trait Widget: Input {
    fn state(&self) -> &WidgetState;
    fn state_mut(&mut self) -> &mut WidgetState;

    /// Total rendered row count when wrapped at `width`. Must be
    /// deterministic for a given `(self, width)` pair — the
    /// scrollback container caches rect decisions against this.
    fn measure(&self, width: u16) -> u16;

    /// Stash own [`Rect`] (`state_mut().rect = area`) and, for
    /// containers, compute + stash each child's rect by recursing.
    /// The default impl only stashes own rect; leaf widgets rely on
    /// it. Containers override to recurse.
    fn layout(&mut self, area: Rect) {
        self.state_mut().rect = area;
    }

    /// Paint into `ctx.area`. The framework guarantees
    /// `ctx.area == self.state().rect` from the most recent
    /// [`layout`](Widget::layout) call.
    fn render(&self, ctx: &mut RenderContext<'_>);

    /// Walk own subtree and push focusable widget ids in tree
    /// pre-order. Default impl: push own `focus_id` if any.
    /// Containers override to recurse into children first.
    fn collect_focusable(&self, out: &mut Vec<FocusId>) {
        if let Some(id) = self.state().focus_id {
            out.push(id);
        }
    }
}
