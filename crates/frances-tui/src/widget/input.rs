//! [`Input`] — the event-handling half of the widget trait split.
//!
//! Split out from `Widget` so Phase C blocks can accept events
//! (e.g. hscroll/vscroll inside the Phase D alt-view inspector)
//! without becoming widgets in their own right.

use crossterm::event::Event;

use super::EventContext;

/// Result of dispatching an event into a widget tree.
pub enum EventOutcome {
    /// Widget handled the event; stop propagation.
    Consumed,
    /// Widget did not handle the event; caller may try elsewhere
    /// (a sibling, an ancestor's fallback handler, the app's
    /// top-level keymap).
    Pass,
}

pub trait Input {
    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome;
}
