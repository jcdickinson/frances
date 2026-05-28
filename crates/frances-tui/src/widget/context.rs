//! Per-frame contexts threaded through widget rendering and event
//! dispatch. Render and event share a [`Focus`] (read-only at render
//! time, mutable while handling events); render also carries a
//! [`FrameTime`] (so animated widgets can pull a wall-clock-paced
//! frame index) and an [`AnimationGate`] (so they can take a lease
//! that tells the host to keep ticking).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{AnimationGate, AnimationLease, Focus, FrameTime, Theme};

pub struct RenderContext<'a> {
    pub area: Rect,
    pub buf: &'a mut Buffer,
    pub theme: &'a Theme,
    pub focus: &'a Focus,
    pub frame_time: &'a dyn FrameTime,
    pub animation: &'a AnimationGate,
}

impl<'a> RenderContext<'a> {
    /// Return a new context targeting `area` but sharing this
    /// context's buffer, theme, focus, frame-time clock, and
    /// animation gate. The borrow on the inner buffer is shortened
    /// to `&mut self` so callers naturally walk children one at a
    /// time.
    pub fn with_area<'s>(&'s mut self, area: Rect) -> RenderContext<'s> {
        RenderContext {
            area,
            buf: &mut *self.buf,
            theme: self.theme,
            focus: self.focus,
            frame_time: self.frame_time,
            animation: self.animation,
        }
    }

    /// Shorthand for `self.animation.lease()`.
    pub fn animation_lease(&self) -> AnimationLease {
        self.animation.lease()
    }
}

pub struct EventContext<'a> {
    pub focus: &'a mut Focus,
    /// Widget sets this if it needs the runloop to redraw before
    /// the next event poll (e.g. animated content). Phase B widgets
    /// don't touch it; reserved for Phase D.
    pub redraw: &'a mut bool,
}
