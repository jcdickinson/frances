//! [`Theme`] — palette of named styles threaded through render +
//! event contexts. Lets widgets pull colours from a single source
//! instead of hardcoding `Color::DarkGray` in each call site.

use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    /// Box-drawing characters for borders.
    pub border: Style,
    /// Title text styling on bordered widgets.
    pub border_title: Style,
    /// "Dim" text — token-status row, secondary annotations.
    pub dim: Style,
    /// Focus highlight.
    pub focused: Style,
}

impl Theme {
    /// Default palette tuned for a dark terminal.
    pub fn default_dark() -> Self {
        Self {
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
