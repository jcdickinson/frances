use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

pub const INPUT_HEIGHT: u16 = 3;

pub struct Textarea {
    lines: Vec<String>,
    /// (line_index, byte_offset within that line)
    cursor: (usize, usize),
    placeholder: String,
}

impl Textarea {
    pub fn new(placeholder: impl Into<String>) -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            placeholder: placeholder.into(),
        }
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = placeholder.into();
    }

    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines_snapshot(&self) -> Vec<String> {
        self.lines.clone()
    }

    pub fn cursor_row(&self) -> u16 {
        self.cursor.0 as u16
    }

    pub fn cursor_display_col(&self) -> u16 {
        let (row, col) = self.cursor;
        display_width(&self.lines[row][..col]) as u16
    }

    pub fn input(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char(c) if !ctrl => {
                self.insert_char(c);
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor.1 = 0;
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor.1 = self.lines[self.cursor.0].len();
            }
            KeyCode::Char('u') if ctrl => {
                let (row, col) = self.cursor;
                self.lines[row].drain(..col);
                self.cursor.1 = 0;
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.cursor.1 = 0,
            KeyCode::End => self.cursor.1 = self.lines[self.cursor.0].len(),
            KeyCode::Enter if alt => self.insert_newline(),
            _ => {}
        }
    }

    fn insert_char(&mut self, c: char) {
        let (row, col) = self.cursor;
        let line = &mut self.lines[row];
        line.insert(col, c);
        self.cursor.1 += c.len_utf8();
    }

    fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let line = &mut self.lines[row];
        let tail = line.split_off(col);
        self.lines.insert(row + 1, tail);
        self.cursor = (row + 1, 0);
    }

    fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let line = &mut self.lines[row];
            let prev = line[..col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.drain(prev..col);
            self.cursor.1 = prev;
        } else if row > 0 {
            let removed = self.lines.remove(row);
            let prev_len = self.lines[row - 1].len();
            self.lines[row - 1].push_str(&removed);
            self.cursor = (row - 1, prev_len);
        }
    }

    fn delete(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].len();
        if col < line_len {
            let next = self.lines[row][col..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| col + i)
                .unwrap_or(line_len);
            self.lines[row].drain(col..next);
        } else if row + 1 < self.lines.len() {
            let next_line = self.lines.remove(row + 1);
            self.lines[row].push_str(&next_line);
        }
    }

    fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let prev = self.lines[row][..col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor.1 = prev;
        } else if row > 0 {
            self.cursor = (row - 1, self.lines[row - 1].len());
        }
    }

    fn move_right(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].len();
        if col < line_len {
            let next = self.lines[row][col..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| col + i)
                .unwrap_or(line_len);
            self.cursor.1 = next;
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
    }

    fn move_up(&mut self) {
        let (row, col) = self.cursor;
        if row > 0 {
            let target_col = clamp_col(&self.lines[row - 1], col);
            self.cursor = (row - 1, target_col);
        }
    }

    fn move_down(&mut self) {
        let (row, col) = self.cursor;
        if row + 1 < self.lines.len() {
            let target_col = clamp_col(&self.lines[row + 1], col);
            self.cursor = (row + 1, target_col);
        }
    }
}

fn clamp_col(line: &str, want_byte_offset: usize) -> usize {
    if want_byte_offset >= line.len() {
        return line.len();
    }
    let mut last = 0usize;
    for (i, _) in line.char_indices() {
        if i > want_byte_offset {
            break;
        }
        last = i;
    }
    last
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}
