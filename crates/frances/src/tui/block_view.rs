use std::io;

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use unicode_width::UnicodeWidthChar;

use super::region::Region;
use super::widget::{RenderCtx, Widget};
use crate::daemon::protocol::BlockKind;

pub struct BlockView<'a> {
    pub kind: BlockKind,
    pub text: &'a str,
}

impl<'a> BlockView<'a> {
    pub fn new(kind: BlockKind, text: &'a str) -> Self {
        Self { kind, text }
    }

    fn prefix(&self) -> &'static str {
        match self.kind {
            BlockKind::UserText => "you: ",
            BlockKind::AssistantText => "frances: ",
        }
    }

    pub fn wrapped_lines(&self, max_width: u16) -> Vec<String> {
        let prefix = self.prefix();
        let indent = " ".repeat(prefix.len());
        let max = max_width.max(1) as usize;

        let mut out = Vec::new();
        for (i, source_line) in self.text.split('\n').enumerate() {
            let lead = if i == 0 { prefix } else { indent.as_str() };
            wrap_into(lead, source_line, max, &mut out);
        }
        if out.is_empty() {
            out.push(prefix.to_string());
        }
        out
    }
}

impl Widget for BlockView<'_> {
    fn measure(&self, max_width: u16) -> u16 {
        self.wrapped_lines(max_width).len() as u16
    }

    fn render(&self, region: Region, ctx: &mut RenderCtx) -> io::Result<()> {
        let lines = self.wrapped_lines(region.width);
        for (i, line) in lines.iter().enumerate() {
            if i as u16 >= region.height {
                break;
            }
            ctx.stdout.queue(MoveTo(region.x, region.y + i as u16))?;
            ctx.stdout.queue(Print(line))?;
            ctx.stdout.queue(Clear(ClearType::UntilNewLine))?;
        }
        Ok(())
    }
}

/// Char-level wrap that respects display width and never breaks mid-character.
/// Pushes one `String` per output row (no trailing newline). Includes the
/// leading prefix on the first row only.
pub fn wrap_into(lead: &str, text: &str, max_width: usize, out: &mut Vec<String>) {
    let mut current = String::from(lead);
    let mut current_width = display_width(lead);

    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + w > max_width && !current.is_empty() && current_width > 0 {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += w;
    }
    out.push(current);
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}
