//! [`TextInput`] — bordered, focusable text input. Wraps
//! `ratatui_textarea::TextArea` for the actual text editing
//! (cursor placement, horizontal scroll, paste handling); this
//! widget owns the surrounding border + optional status-title
//! inset.
//!
//! `TextArea` paints its own reversed-style cursor cell, so the
//! terminal cursor stays hidden upstream — same model as the
//! pre-Phase-B `FooterBlock`.

use crossterm::event::Event;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::TextArea;

use super::{EventContext, EventOutcome, FocusManager, Input, RenderContext, Widget, WidgetState};

/// Total rows occupied by a single-line `TextInput`: top + bottom
/// border = 2 + 1 row of text. Mirrors the pre-Phase-B
/// `INPUT_HEIGHT` const from `crates/frances/src/tui/textarea.rs`.
pub const TEXT_INPUT_HEIGHT: u16 = 3;

pub struct TextInput {
    textarea: TextArea<'static>,
    status: Option<String>,
    state: WidgetState,
}

impl TextInput {
    pub fn new(focus: &mut FocusManager, placeholder: impl Into<String>) -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text(placeholder);
        // The cursor cell stays at the widget's default reversed
        // style, but the whole-line underline is too noisy for a
        // single-row input box.
        textarea.set_cursor_line_style(Style::default());
        Self {
            textarea,
            status: None,
            state: WidgetState {
                focus_id: Some(focus.allocate()),
                ..WidgetState::default()
            },
        }
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    pub fn clear(&mut self) {
        self.textarea.clear();
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.textarea.set_placeholder_text(placeholder);
    }

    /// Set the status text inset on the top border (e.g.
    /// `┌─ streaming… ─────┐`). `None` clears it.
    pub fn set_status(&mut self, status: Option<impl Into<String>>) {
        self.status = status.map(Into::into);
    }
}

impl Input for TextInput {
    fn handle_event(&mut self, _: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        match event {
            Event::Key(k) => {
                // ratatui-textarea 0.9 ships `From<crossterm::event::KeyEvent>`
                // for its Input type behind the (default-on)
                // `crossterm` feature. Drops the entire hand-translation
                // table that lived in the pre-Phase-B `Textarea` wrapper.
                let _ = self.textarea.input(*k);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Widget for TextInput {
    fn state(&self) -> &WidgetState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    fn measure(&self, _: u16) -> u16 {
        TEXT_INPUT_HEIGHT
    }

    fn render(&self, ctx: &mut RenderContext<'_>) {
        if ctx.area.width == 0 || ctx.area.height == 0 {
            return;
        }
        // Re-apply the border each frame so the status-title text
        // can change between frames without redoing the textarea's
        // internal state. The clone is cheap — TextArea owns Strings
        // and styles; no internal handles or animations.
        let block = match self.status.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => Block::default()
                .borders(Borders::ALL)
                .style(ctx.theme.border)
                .title(Line::from(vec![
                    Span::raw("─ "),
                    Span::styled(s.to_string(), ctx.theme.status),
                    Span::raw(" "),
                ])),
            None => Block::default()
                .borders(Borders::ALL)
                .style(ctx.theme.border),
        };
        let mut snapshot = self.textarea.clone();
        snapshot.set_block(block);
        ratatui::widgets::Widget::render(&snapshot, ctx.area, &mut *ctx.buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{Focus, Theme};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn type_chars(input: &mut TextInput, text: &str) {
        let mut focus = Focus::new();
        let mut redraw = false;
        let mut ctx = EventContext {
            focus: &mut focus,
            redraw: &mut redraw,
        };
        for c in text.chars() {
            let ev = Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(c),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            input.handle_event(&mut ctx, &ev);
        }
    }

    #[test]
    fn empty_input_text_round_trips() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "type a message");
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn typed_chars_appear_in_text() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        type_chars(&mut input, "hello");
        assert_eq!(input.text(), "hello");
        assert!(!input.is_empty());
    }

    #[test]
    fn clear_empties_textarea() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        type_chars(&mut input, "abc");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn measure_is_three_rows() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "hi");
        assert_eq!(input.measure(80), TEXT_INPUT_HEIGHT);
    }

    #[test]
    fn focus_id_is_allocated_on_construction() {
        let mut mgr = FocusManager::new();
        let input = TextInput::new(&mut mgr, "hi");
        assert!(input.state().focus_id.is_some());
    }

    #[test]
    fn render_paints_a_border() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        let area = Rect::new(0, 0, 10, 3);
        input.layout(area);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame: 0,
        };
        input.render(&mut ctx);
        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(9, 0)].symbol(), "┐");
        assert_eq!(buf[(0, 2)].symbol(), "└");
        assert_eq!(buf[(9, 2)].symbol(), "┘");
    }

    #[test]
    fn status_title_lands_on_top_border() {
        let mut mgr = FocusManager::new();
        let mut input = TextInput::new(&mut mgr, "hi");
        input.set_status(Some("streaming"));
        let area = Rect::new(0, 0, 20, 3);
        input.layout(area);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let focus = Focus::new();
        let mut ctx = RenderContext {
            area,
            buf: &mut buf,
            theme: &theme,
            focus: &focus,
            frame: 0,
        };
        input.render(&mut ctx);
        // Top border row contains "─ streaming " somewhere after the
        // left corner. Concatenate the row and check.
        let row: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row.contains("streaming"),
            "expected status text on top border, got `{row}`"
        );
    }
}
