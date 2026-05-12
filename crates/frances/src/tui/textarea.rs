use std::io;

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use unicode_width::UnicodeWidthChar;

use super::region::Region;
use super::widget::{RenderCtx, Widget};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
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
            // delete previous char (UTF-8 aware)
            let line = &mut self.lines[row];
            let prev = line[..col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.drain(prev..col);
            self.cursor.1 = prev;
        } else if row > 0 {
            // join with previous line
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

    fn cursor_display_col(&self) -> u16 {
        let (row, col) = self.cursor;
        display_width(&self.lines[row][..col]) as u16
    }
}

impl Widget for Textarea {
    fn measure(&self, _max_width: u16) -> u16 {
        INPUT_HEIGHT
    }

    fn render(&self, region: Region, ctx: &mut RenderCtx) -> io::Result<()> {
        if region.height < 3 || region.width < 2 {
            return Ok(());
        }
        let inner_width = region.width.saturating_sub(2) as usize;

        // Top border
        let top = format!("┌{}┐", "─".repeat(inner_width));
        ctx.stdout.queue(MoveTo(region.x, region.y))?;
        ctx.stdout.queue(Print(&top))?;

        // Content row(s) — render each editor line on its own row, padded.
        let content_row_count = region.height.saturating_sub(2);
        for i in 0..content_row_count {
            let row = region.y + 1 + i;
            ctx.stdout.queue(MoveTo(region.x, row))?;
            ctx.stdout.queue(Print("│"))?;
            let idx = i as usize;
            let line_str = if self.is_empty() && idx == 0 {
                pad_to_width(&self.placeholder, inner_width)
            } else if idx < self.lines.len() {
                pad_to_width(&self.lines[idx], inner_width)
            } else {
                " ".repeat(inner_width)
            };
            ctx.stdout.queue(Print(line_str))?;
            ctx.stdout.queue(Print("│"))?;
        }

        // Bottom border
        let bottom = format!("└{}┘", "─".repeat(inner_width));
        ctx.stdout
            .queue(MoveTo(region.x, region.y + region.height - 1))?;
        ctx.stdout.queue(Print(&bottom))?;

        // Position cursor inside the box, on the active line.
        let active_row = (self.cursor.0 as u16).min(content_row_count.saturating_sub(1));
        let cursor_x = region.x + 1 + self.cursor_display_col();
        let cursor_y = region.y + 1 + active_row;
        ctx.stdout.queue(MoveTo(cursor_x, cursor_y))?;
        // Force any leftover content beyond what we wrote to clear
        let _ = Clear(ClearType::UntilNewLine); // noop; our pad_to_width already covers
        Ok(())
    }
}

fn clamp_col(line: &str, want_byte_offset: usize) -> usize {
    if want_byte_offset >= line.len() {
        return line.len();
    }
    // Walk to a valid char boundary at or before want_byte_offset.
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

fn pad_to_width(s: &str, target_width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > target_width {
            break;
        }
        out.push(c);
        used += w;
    }
    if used < target_width {
        out.push_str(&" ".repeat(target_width - used));
    }
    out
}
