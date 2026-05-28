//! `[label]` status-pill rendering, shared between block headers
//! (shell, reasoning) and the input-status row in the footer.

use ratatui::style::{Color, Modifier, Style};

/// Semantic colouring for a status pill. The text inside the brackets
/// is content-specific; the tone fixes the colour family so disparate
/// surfaces (shell exit code, model reasoning state, footer status)
/// stay visually consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    /// In-flight work — cyan. Shell `Running`, reasoning `Streaming`.
    Pending,
    /// Completed work that went well — green. Shell `Success`.
    Success,
    /// Completed work that didn't — red. Shell `Exit(n)`.
    Failure,
    /// Completed work, neutral — dim. Reasoning `Done`.
    Settled,
}

impl StatusTone {
    pub fn style(self) -> Style {
        match self {
            StatusTone::Pending => Style::default().fg(Color::Cyan),
            StatusTone::Success => Style::default().fg(Color::Green),
            StatusTone::Failure => Style::default().fg(Color::Red),
            StatusTone::Settled => Style::default().add_modifier(Modifier::DIM),
        }
    }
}

/// Format `[label] ` with the tone's style. Includes the trailing
/// space so callers can concatenate the pill directly onto a body
/// without re-padding.
pub fn status_prefix(label: &str, tone: StatusTone) -> (String, Style) {
    (format!("[{label}] "), tone.style())
}
