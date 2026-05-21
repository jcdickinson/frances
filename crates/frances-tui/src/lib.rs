//! frances-tui — terminal UI building blocks for the frances binary.
//!
//! The headline piece is [`ScrollbackBackend`], a `ratatui::backend::Backend`
//! wrapper that paints into an inline band of the real terminal whose
//! height can change at runtime — what `ratatui::Viewport::Inline`
//! would be if its height weren't fixed at construction. The band is
//! anchored at the cursor's row at launch and grows downward as the
//! UI accumulates content; once the band hits the screen bottom,
//! further growth shifts the top upward (scrolling pre-existing
//! content into native scrollback).
//!
//! On top of that, [`ScrollbackContainer`] is the layout primitive
//! that holds an append-only list of history [`Block`]s plus a single
//! footer block, decides which rows fit, and drives the backend. It
//! also owns the scrollback inspector — `set_scrollback` +
//! `scroll_up` / `scroll_down` + `paint_scrollback` render the
//! container's full block history into an alt-screen view.

pub mod block;
pub mod measured_widget;
pub mod scrollback_backend;
pub mod scrollback_container;
pub mod widget;

pub use block::{Block, TruncatedBlock};
pub use measured_widget::MeasuredWidget;
pub use scrollback_backend::{ScrollbackBackend, SyncGuard};
pub use scrollback_container::{BlockId, ScrollbackContainer};
pub use widget::{
    EventContext, EventOutcome, Focus, FocusId, FocusManager, Input, RenderContext, Theme, Widget,
    WidgetState,
};
