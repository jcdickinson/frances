use frances_daemon::protocol::{BlockKind, ShellState};
use frances_tui::Block;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthChar;

use super::textarea::INPUT_HEIGHT;

/// History row for a labelled (kind + text) block. Wraps to the
/// available width with the kind prefix on the first row and a
/// matching-width indent on continuation rows.
pub struct LabelledBlock {
    pub kind: BlockKind,
    pub text: String,
}

impl LabelledBlock {
    pub fn new(kind: BlockKind, text: String) -> Self {
        Self { kind, text }
    }
}

impl Block for LabelledBlock {
    fn measure(&self, width: u16) -> u16 {
        wrapped_block_lines(&self.kind, &self.text, width).len() as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = wrapped_block_lines(&self.kind, &self.text, area.width);
        let prefix = prefix_for(&self.kind);
        let prefix_bytes = prefix.len();
        let prefix_cols = display_width(&prefix) as u16;
        let prefix_style = prefix_style(&self.kind);
        for (i, line) in lines.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let y = area.y + i as u16;
            if i == 0 && line.starts_with(&prefix) {
                buf.set_string(area.x, y, &line[..prefix_bytes], prefix_style);
                if line.len() > prefix_bytes {
                    buf.set_string(
                        area.x + prefix_cols,
                        y,
                        &line[prefix_bytes..],
                        Style::default(),
                    );
                }
            } else {
                buf.set_string(area.x, y, line, Style::default());
            }
        }
    }
}

/// History row that holds raw, pre-formatted lines and renders them
/// verbatim (no kind prefix, no re-wrap). Used for banner rows, usage
/// summaries, error / approval messages — anything not driven by the
/// daemon's [`BlockKind`] protocol that still wants to live in the
/// container's scrollback. `style` paints the whole block uniformly;
/// ANSI variants only by convention (RGB stays available for future
/// syntax-highlighted block types).
pub struct RawBlock {
    pub lines: Vec<String>,
    pub style: Style,
}

impl RawBlock {
    pub fn single_styled(line: String, style: Style) -> Self {
        Self {
            lines: vec![line],
            style,
        }
    }
}

impl Block for RawBlock {
    fn measure(&self, _width: u16) -> u16 {
        self.lines.len() as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        for (i, line) in self.lines.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            buf.set_string(area.x, area.y + i as u16, line, self.style);
        }
    }
}

/// Footer block: a bordered textarea snapshot. The cursor inside the
/// textarea is positioned separately by the main loop, after the
/// container draw. `status`, when present, is rendered inside the
/// top border as `┌─ {status} ──…─┐` — used to surface a streaming
/// indicator while an LLM stream is in flight.
pub struct FooterBlock {
    pub textarea_lines: Vec<String>,
    pub placeholder: String,
    pub status: Option<String>,
}

impl Block for FooterBlock {
    fn measure(&self, _width: u16) -> u16 {
        INPUT_HEIGHT
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let textarea_h = INPUT_HEIGHT.min(area.height);
        if textarea_h < 3 || area.width < 2 {
            return;
        }
        let inner_w = area.width.saturating_sub(2) as usize;
        // Top border with optional status inset. The status text is
        // dimmed so it reads as chrome rather than content.
        buf.set_string(area.x, area.y, "┌", Style::default());
        buf.set_string(area.x + 1 + inner_w as u16, area.y, "┐", Style::default());
        if let Some(status) = self.status.as_deref().filter(|s| !s.is_empty()) {
            // Layout: `─ {status} ` then fill the rest with `─`.
            // Truncate the status if it wouldn't leave any fill room.
            let max_status_cols = inner_w.saturating_sub(4); // ` ` + status + ` ` + at least 1 fill
            let truncated = truncate_to_width(status, max_status_cols);
            let status_cols = display_width(&truncated);
            // Leading single `─`, space, status, space, fill `─`s.
            let mut x = area.x + 1;
            buf.set_string(x, area.y, "─", Style::default());
            x += 1;
            buf.set_string(x, area.y, " ", Style::default());
            x += 1;
            buf.set_string(x, area.y, &truncated, Style::default().fg(Color::DarkGray));
            x += status_cols as u16;
            buf.set_string(x, area.y, " ", Style::default());
            x += 1;
            let consumed = (x - (area.x + 1)) as usize;
            let fill = inner_w.saturating_sub(consumed);
            if fill > 0 {
                buf.set_string(x, area.y, "─".repeat(fill), Style::default());
            }
        } else {
            buf.set_string(area.x + 1, area.y, "─".repeat(inner_w), Style::default());
        }
        let bottom = format!("└{}┘", "─".repeat(inner_w));
        buf.set_string(area.x, area.y + textarea_h - 1, bottom, Style::default());

        let content_rows = textarea_h - 2;
        let placeholder_active = self.textarea_lines.iter().all(|l| l.is_empty());
        for i in 0..content_rows {
            let row = area.y + 1 + i;
            buf.set_string(area.x, row, "│", Style::default());
            let line_str = if placeholder_active && i == 0 {
                pad_to_width(&self.placeholder, inner_w)
            } else if (i as usize) < self.textarea_lines.len() {
                pad_to_width(&self.textarea_lines[i as usize], inner_w)
            } else {
                " ".repeat(inner_w)
            };
            let line_style = if placeholder_active && i == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            buf.set_string(area.x + 1, row, &line_str, line_style);
            buf.set_string(area.x + 1 + inner_w as u16, row, "│", Style::default());
        }
    }
}

pub fn prefix_for(kind: &BlockKind) -> String {
    match kind {
        BlockKind::Text { sender: Some(s) } => format!("{s}: "),
        BlockKind::Text { sender: None } => String::new(),
        BlockKind::ToolUse { name } => format!("→ {name}("),
        BlockKind::ShellOutput { state } => match state {
            ShellState::Running => "[…] ".to_string(),
            ShellState::Success => "[ok] ".to_string(),
            ShellState::Exit(n) => format!("[exit {n}] "),
        },
    }
}

fn prefix_style(kind: &BlockKind) -> Style {
    match kind {
        BlockKind::Text { .. } => Style::default(),
        BlockKind::ToolUse { .. } => Style::default().fg(Color::Yellow),
        BlockKind::ShellOutput { state } => match state {
            ShellState::Running => Style::default().fg(Color::Cyan),
            ShellState::Success => Style::default().fg(Color::Green),
            ShellState::Exit(_) => Style::default().fg(Color::Red),
        },
    }
}

pub fn wrapped_block_lines(kind: &BlockKind, text: &str, width: u16) -> Vec<String> {
    let prefix = prefix_for(kind);
    let indent = " ".repeat(display_width(&prefix));
    let max = width.max(1) as usize;
    // LLM completions routinely end with one or more trailing `\n`s.
    // Without stripping, `split('\n')` yields an empty trailing element
    // that renders as an indent-only continuation row — visually a
    // blank line between this block and whatever comes next. Embedded
    // blank lines (`\n\n` mid-text) are preserved as real paragraph
    // breaks.
    let text = text.trim_end_matches('\n');

    let mut out = Vec::new();
    for (i, source_line) in text.split('\n').enumerate() {
        let lead = if i == 0 {
            prefix.as_str()
        } else {
            indent.as_str()
        };
        wrap_into(lead, source_line, max, &mut out);
    }
    if out.is_empty() {
        out.push(prefix);
    }
    out
}

fn wrap_into(lead: &str, text: &str, max_width: usize, out: &mut Vec<String>) {
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

fn truncate_to_width(s: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max_cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frances() -> BlockKind {
        BlockKind::Text {
            sender: Some("frances".into()),
        }
    }

    #[test]
    fn no_trailing_newline_is_unchanged() {
        let lines = wrapped_block_lines(&frances(), "Hello", 80);
        assert_eq!(lines, vec!["frances: Hello"]);
    }

    #[test]
    fn single_trailing_newline_is_stripped() {
        let lines = wrapped_block_lines(&frances(), "Hello\n", 80);
        assert_eq!(
            lines,
            vec!["frances: Hello"],
            "trailing `\\n` should not produce an indent-only continuation row"
        );
    }

    #[test]
    fn multiple_trailing_newlines_are_stripped() {
        let lines = wrapped_block_lines(&frances(), "Hello\n\n\n", 80);
        assert_eq!(lines, vec!["frances: Hello"]);
    }

    #[test]
    fn mid_text_paragraph_break_is_preserved() {
        let lines = wrapped_block_lines(&frances(), "One\n\nTwo", 80);
        assert_eq!(
            lines,
            vec![
                "frances: One".to_string(),
                "         ".to_string(),
                "         Two".to_string(),
            ],
            "an internal `\\n\\n` is a real paragraph break and stays"
        );
    }

    #[test]
    fn mid_text_paragraph_break_with_trailing_newline_keeps_only_the_break() {
        let lines = wrapped_block_lines(&frances(), "One\n\nTwo\n", 80);
        assert_eq!(
            lines,
            vec![
                "frances: One".to_string(),
                "         ".to_string(),
                "         Two".to_string(),
            ]
        );
    }

    #[test]
    fn newline_only_text_collapses_to_just_the_prefix() {
        let lines = wrapped_block_lines(&frances(), "\n", 80);
        assert_eq!(lines, vec!["frances: "]);
    }

    #[test]
    fn senderless_text_block_with_trailing_newline_does_not_emit_blank_row() {
        let kind = BlockKind::Text { sender: None };
        let lines = wrapped_block_lines(&kind, "Hello\n", 80);
        assert_eq!(lines, vec!["Hello"]);
    }
}
