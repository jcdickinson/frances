use ratatui_textarea::TextArea;

pub const INPUT_HEIGHT: u16 = 3;

pub struct Textarea {
    textarea: TextArea<'static>,
}

impl Textarea {
    pub fn new(placeholder: impl Into<String>) -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text(placeholder);
        Self { textarea }
    }

    pub fn clear(&mut self) {
        self.textarea.clear();
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.textarea.set_placeholder_text(placeholder);
    }

    pub fn placeholder(&self) -> String {
        self.textarea.placeholder().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn lines_snapshot(&self) -> Vec<String> {
        self.textarea.lines().to_vec()
    }

    pub fn cursor_row(&self) -> u16 {
        self.textarea.cursor().0 as u16
    }

    pub fn cursor_display_col(&self) -> u16 {
        self.textarea.cursor().1 as u16
    }

    pub fn input(&mut self, key: crossterm::event::KeyEvent) {
        use ratatui_textarea::Input;
        use crossterm::event::KeyCode;

        let ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
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

