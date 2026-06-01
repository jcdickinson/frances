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
    /// Return a new context targeting `area`, sharing this context's
    /// buffer, theme, focus, frame-time clock, and animation gate.
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

    pub fn animation_lease(&self) -> AnimationLease {
        self.animation.lease()
    }
}

pub struct EventContext<'a> {
    pub focus: &'a mut Focus,
    /// Widget sets this if it needs the runloop to redraw before the next event poll.
    pub redraw: &'a mut bool,
}
