//! Per-frame contexts threaded through widget rendering and event
//! dispatch. Render and event share a [`Focus`] (read-only at render
//! time, mutable while handling events) and a frame counter.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Focus, Theme};

pub struct RenderContext<'a> {
    pub area: Rect,
    pub buf: &'a mut Buffer,
    pub theme: &'a Theme,
    pub focus: &'a Focus,
    pub frame: u64,
}

impl<'a> RenderContext<'a> {
    /// Return a new context targeting `area` but sharing this
    /// context's buffer, theme, focus, and frame counter. The
    /// borrow on the inner buffer is shortened to `&mut self` so
    /// callers naturally walk children one at a time.
    pub fn with_area<'s>(&'s mut self, area: Rect) -> RenderContext<'s> {
        RenderContext {
            area,
            buf: &mut *self.buf,
            theme: self.theme,
            focus: self.focus,
            frame: self.frame,
        }
    }
}

pub struct EventContext<'a> {
    pub focus: &'a mut Focus,
    /// Widget sets this if it needs the runloop to redraw before
    /// the next event poll (e.g. animated content). Phase B widgets
    /// don't touch it; reserved for Phase D.
    pub redraw: &'a mut bool,
}
