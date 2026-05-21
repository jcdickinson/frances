//! [`Theme`] — palette of named styles threaded through render +
//! event contexts. Lets widgets pull colours from a single source
//! instead of hardcoding `Color::DarkGray` in each call site.

use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    /// Streaming-status text inset on a [`TextInput`](super::TextInput)
    /// border title.
    pub status: Style,
    /// Box-drawing characters for borders.
    pub border: Style,
    /// Title text styling on bordered widgets.
    pub border_title: Style,
    /// "Dim" text — token-status row, secondary annotations.
    pub dim: Style,
    /// Focus highlight; reserved for Phase D widgets that want to
    /// visually indicate the focused state.
    pub focused: Style,
}

impl Theme {
    /// Default palette tuned for a dark terminal.
    pub fn default_dark() -> Self {
        Self {
            status: Style::default().fg(Color::DarkGray),
            border: Style::default(),
            border_title: Style::default(),
            dim: Style::default().fg(Color::DarkGray),
            focused: Style::default().add_modifier(Modifier::REVERSED),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}
