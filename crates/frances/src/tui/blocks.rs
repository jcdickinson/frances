use std::sync::Arc;

use frances_daemon::protocol::{BlockKind, ShellState};
use frances_tui::Block;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use super::textarea::INPUT_HEIGHT;

const TOKEN_STATUS_HEIGHT: u16 = 1;

/// Maximum body lines (post trailing-newline strip) shown for a shell
/// output block. Earlier lines are collapsed into a single `… [N earlier
/// lines]` marker so the visible tail tracks the action.
const SHELL_TAIL_LINES: usize = 10;

/// Build the right [`Block`] impl for a wire `BlockKind` + accumulated
/// text. Most kinds map onto a generic [`LabelledBlock`]; `ShellOutput`
/// has its own structural shape (header + body tail) and gets a
/// dedicated [`ShellOutputBlock`].
pub fn block_for_kind(kind: BlockKind, text: String) -> Box<dyn Block> {
    match kind {
        BlockKind::ShellOutput { state, cmd } => Box::new(ShellOutputBlock::new(state, cmd, text)),
        BlockKind::Diff { lines } => Box::new(DiffBlock::new(lines)),
        BlockKind::ToolUse {
            name,
            detail: Some(detail),
        } => Box::new(ToolUseBlock::new(name, detail)),
        other => Box::new(LabelledBlock::new(other, text)),
    }
}

pub struct DiffBlock {
    lines: Vec<frances_daemon::protocol::DiffLine>,
}

impl DiffBlock {
    pub fn new(lines: Vec<frances_daemon::protocol::DiffLine>) -> Self {
        Self { lines }
    }
}

impl Block for DiffBlock {
    fn measure(&self, width: u16) -> u16 {
        let max = width.max(1) as usize;
        let mut count = 0;
        for line in &self.lines {
            let content = match line {
                frances_daemon::protocol::DiffLine::Context { text: c, line: l } => {
                    format!("{:4} {}", l, c)
                }
                frances_daemon::protocol::DiffLine::Added(a) => a.to_string(),
                frances_daemon::protocol::DiffLine::Removed(r) => r.to_string(),
            };
            let mut out = Vec::new();
            wrap_into("", &content, max, &mut out);
            count += out.len().max(1) as u16;
        }
        count
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut row = 0u16;
        let max = area.width.max(1) as usize;
        for line in &self.lines {
            let (content, style) = match line {
                frances_daemon::protocol::DiffLine::Context { text: c, line: l } => {
                    let formatted = format!("{:4} {}", l, c);
                    (formatted, Style::default())
                }
                frances_daemon::protocol::DiffLine::Added(a) => (
                    a.to_string(),
                    Style::default().bg(Color::Green).fg(Color::Black),
                ),
                frances_daemon::protocol::DiffLine::Removed(r) => (
                    r.to_string(),
                    Style::default().bg(Color::Red).fg(Color::Black),
                ),
            };

            let mut out = Vec::new();
            wrap_into("", &content, max, &mut out);

            for wrapped_line in out {
                if row >= area.height {
                    return;
                }
                buf.set_string(area.x, area.y + row, &wrapped_line, style);
                let w = display_width(&wrapped_line) as u16;
                if w < area.width {
                    buf.set_string(
                        area.x + w,
                        area.y + row,
                        " ".repeat((area.width - w) as usize),
                        style,
                    );
                }
                row += 1;
            }
        }
    }
}

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

/// History row for a shell command's output. Renders as:
///   `[state] cmd` (the cmd may wrap; continuation rows are unindented)
///   `… [N earlier lines]` (only when the body is longer than the tail)
///   last-`SHELL_TAIL_LINES` body lines (wrapped, unindented)
///
/// `state` drives the prefix label and its colour; `cmd` rides on every
/// `BlockDelta` so the header stays pinned even while the body keeps
/// streaming.
pub struct ShellOutputBlock {
    pub state: ShellState,
    pub cmd: Arc<str>,
    pub text: String,
}

impl ShellOutputBlock {
    pub fn new(state: ShellState, cmd: Arc<str>, text: String) -> Self {
        Self { state, cmd, text }
    }

    fn header_prefix(&self) -> String {
        shell_state_prefix(&self.state)
    }

    fn header_lines(&self, width: u16) -> Vec<String> {
        let prefix = self.header_prefix();
        let mut out = Vec::new();
        wrap_into(&prefix, &self.cmd, width.max(1) as usize, &mut out);
        out
    }

    /// Body rows: ellipsis marker (if any) plus the last
    /// `SHELL_TAIL_LINES` source lines, each wrapped to width. Returns
    /// an empty Vec when there is no body content beyond the cmd.
    fn body_lines(&self, width: u16) -> Vec<String> {
        let mut source: Vec<&str> = self.text.split('\n').collect();
        // Drop trailing empty entries from a trailing `\n` so we don't
        // burn budget rendering a blank tail row.
        while matches!(source.last(), Some(&"")) {
            source.pop();
        }
        if source.is_empty() {
            return Vec::new();
        }

        let max = width.max(1) as usize;
        let mut out = Vec::new();
        if source.len() > SHELL_TAIL_LINES {
            let omitted = source.len() - SHELL_TAIL_LINES;
            let marker = format!(
                "… [{omitted} earlier line{}]",
                if omitted == 1 { "" } else { "s" }
            );
            wrap_into("", &marker, max, &mut out);
            for line in &source[source.len() - SHELL_TAIL_LINES..] {
                wrap_into("", line, max, &mut out);
            }
        } else {
            for line in &source {
                wrap_into("", line, max, &mut out);
            }
        }
        out
    }
}

impl Block for ShellOutputBlock {
    fn measure(&self, width: u16) -> u16 {
        (self.header_lines(width).len() + self.body_lines(width).len()) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let header = self.header_lines(area.width);
        let body = self.body_lines(area.width);
        let prefix = self.header_prefix();
        let prefix_bytes = prefix.len();
        let prefix_cols = display_width(&prefix) as u16;
        let prefix_style = shell_state_prefix_style(&self.state);

        let mut row = 0u16;
        for (i, line) in header.iter().enumerate() {
            if row >= area.height {
                return;
            }
            let y = area.y + row;
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
            row += 1;
        }
        for line in body.iter() {
            if row >= area.height {
                return;
            }
            buf.set_string(area.x, area.y + row, line, Style::default());
            row += 1;
        }
    }
}

/// History row for a one-shot tool call with a detail suffix. Renders
/// as `→ {name}  {detail}` on one line — the prefix in yellow, the
/// detail in dim — wrapping `detail` to subsequent rows (still dim,
/// indented to match the prefix column) when the line overflows.
///
/// The plain `BlockKind::ToolUse` variant (no detail) still routes
/// through [`LabelledBlock`]; only the `Some(detail)` shape comes here.
pub struct ToolUseBlock {
    name: Arc<str>,
    detail: Arc<str>,
}

impl ToolUseBlock {
    pub fn new(name: Arc<str>, detail: Arc<str>) -> Self {
        Self { name, detail }
    }

    fn name_prefix(&self) -> String {
        format!("→ {}  ", self.name)
    }

    fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let max = width.max(1) as usize;
        let prefix = self.name_prefix();
        let indent = " ".repeat(display_width(&prefix));
        let mut out = Vec::new();
        for (i, source_line) in self.detail.split('\n').enumerate() {
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
}

impl Block for ToolUseBlock {
    fn measure(&self, width: u16) -> u16 {
        self.wrapped_lines(width).len() as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.wrapped_lines(area.width);
        let prefix = self.name_prefix();
        // Split the prefix into the colored arrow+name segment (yellow)
        // and the trailing two spaces that bridge into the dim detail.
        // The arrow+name byte count is `prefix.len() - 2` because the
        // suffix is exactly `"  "` (two ASCII spaces).
        let arrow_bytes = prefix.len() - 2;
        let prefix_cols = display_width(&prefix) as u16;
        let arrow_style = Style::default().fg(Color::Yellow);
        let dim_style = Style::default().add_modifier(Modifier::DIM);
        for (i, line) in lines.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let y = area.y + i as u16;
            if i == 0 && line.starts_with(&prefix) {
                buf.set_string(area.x, y, &line[..arrow_bytes], arrow_style);
                if line.len() > prefix.len() {
                    buf.set_string(area.x + prefix_cols, y, &line[prefix.len()..], dim_style);
                }
            } else {
                buf.set_string(area.x, y, line, dim_style);
            }
        }
    }
}

fn shell_state_prefix(state: &ShellState) -> String {
    match state {
        ShellState::Running => "[…] ".to_string(),
        ShellState::Success => "[ok] ".to_string(),
        ShellState::Exit(n) => format!("[exit {n}] "),
    }
}

fn shell_state_prefix_style(state: &ShellState) -> Style {
    match state {
        ShellState::Running => Style::default().fg(Color::Cyan),
        ShellState::Success => Style::default().fg(Color::Green),
        ShellState::Exit(_) => Style::default().fg(Color::Red),
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

/// Footer block: a bordered textarea snapshot followed by a one-row
/// token status area below the input. The cursor inside the textarea is
/// positioned separately by the main loop, after the container draw.
/// `status`, when present, is rendered inside the top border as
/// `┌─ {status} ──…─┐` — used to surface a streaming indicator while
/// an LLM stream is in flight.
pub struct FooterBlock {
    pub textarea_lines: Vec<String>,
    pub placeholder: String,
    pub status: Option<String>,
    pub token_status: Option<String>,
}

impl Block for FooterBlock {
    fn measure(&self, _width: u16) -> u16 {
        INPUT_HEIGHT + TOKEN_STATUS_HEIGHT
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

        let status_row = area.y + textarea_h;
        if status_row < area.y + area.height {
            let text = self.token_status.as_deref().unwrap_or("tokens: —");
            let status = pad_to_width(text, area.width as usize);
            buf.set_string(
                area.x,
                status_row,
                status,
                Style::default().fg(Color::DarkGray),
            );
        }
    }
}

pub fn prefix_for(kind: &BlockKind) -> String {
    match kind {
        BlockKind::Text { sender: Some(s) } => format!("{s}: "),
        BlockKind::Text { sender: None } => String::new(),
        // The `detail`-bearing variant routes through `ToolUseBlock`; the
        // `LabelledBlock` path only sees plain tool-use markers.
        BlockKind::ToolUse { name, .. } => format!("→ {name}"),
        BlockKind::ShellOutput { .. } => {
            // ShellOutput renders through ShellOutputBlock, which owns
            // its own prefix; LabelledBlock should never see this kind.
            String::new()
        }
        BlockKind::Diff { .. } => String::new(),
    }
}

fn prefix_style(kind: &BlockKind) -> Style {
    match kind {
        BlockKind::Text { .. } => Style::default(),
        BlockKind::ToolUse { .. } => Style::default().fg(Color::Yellow),
        BlockKind::ShellOutput { .. } => Style::default(),
        BlockKind::Diff { .. } => Style::default(),
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
