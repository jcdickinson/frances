use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::TextArea;

pub const INPUT_HEIGHT: u16 = 3;

pub struct Textarea {
    textarea: TextArea<'static>,
}

impl Textarea {
    pub fn new(placeholder: impl Into<String>) -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text(placeholder);
        // The cursor cell stays at the widget's default reversed
        // style, but the whole-line underline is too noisy for a
        // single-row input box.
        textarea.set_cursor_line_style(Style::default());
        Self { textarea }
    }

    pub fn clear(&mut self) {
        self.textarea.clear();
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.textarea.set_placeholder_text(placeholder);
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Clone the inner widget with a bordered `Block` applied, plus an
    /// optional dimmed status inset on the top border (e.g.
    /// `┌─ streaming… ─────┐`). The clone lets the footer composite
    /// own a renderable widget snapshot without borrowing from the
    /// live editor.
    pub fn snapshot_widget(&self, status: Option<&str>) -> TextArea<'static> {
        let mut snapshot = self.textarea.clone();
        let block = match status.filter(|s| !s.is_empty()) {
            Some(status) => Block::default()
                .borders(Borders::ALL)
                .title(Line::from(vec![
                    Span::raw("─ "),
                    Span::styled(status.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                ])),
            None => Block::default().borders(Borders::ALL),
        };
        snapshot.set_block(block);
        snapshot
    }

    pub fn input(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        use ratatui_textarea::Input;

        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(crossterm::event::KeyModifiers::ALT);

        let input = match key.code {
            KeyCode::Char(c) => Input {
                key: ratatui_textarea::Key::Char(c),
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Backspace => Input {
                key: ratatui_textarea::Key::Backspace,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Delete => Input {
                key: ratatui_textarea::Key::Delete,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Left => Input {
                key: ratatui_textarea::Key::Left,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Right => Input {
                key: ratatui_textarea::Key::Right,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Up => Input {
                key: ratatui_textarea::Key::Up,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Down => Input {
                key: ratatui_textarea::Key::Down,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Home => Input {
                key: ratatui_textarea::Key::Home,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::End => Input {
                key: ratatui_textarea::Key::End,
                ctrl,
                alt,
                shift: false,
            },
            KeyCode::Enter => {
                if alt {
                    Input {
                        key: ratatui_textarea::Key::Enter,
                        ctrl,
                        alt: true,
                        shift: false,
                    }
                } else {
                    Input {
                        key: ratatui_textarea::Key::Enter,
                        ctrl,
                        alt,
                        shift: false,
                    }
                }
            }
            _ => return,
        };

        self.textarea.input(input);
    }
}
