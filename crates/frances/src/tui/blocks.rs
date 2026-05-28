use std::sync::Arc;

use crate::tui::status::{StatusTone, status_prefix};
use frances_session::events::{
    BlockKind as WireBlockKind, ReasoningState, ShellState, Source, TailedHeader,
};
use frances_tui::widget::{EventContext, EventOutcome, Input};
use frances_tui::{Block, BlockKind, BlockMeasureContext, BlockRenderContext};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

/// Maximum body lines (post trailing-newline strip) shown for a
/// tailed block in its compact (unfocused) state. Earlier lines are
/// collapsed into a single `… [N earlier lines]` marker so the visible
/// tail tracks the action.
const TAIL_LINES: usize = 10;

/// Expanded tail height when a tailed block is the alt-view inspector's
/// selection. Doubles the visible body so the user has room to see more
/// of the source while paging with `j`/`k`/`u`/`d`.
const TAIL_LINES_FOCUSED: usize = 20;

fn tail_lines_for(selected: bool) -> usize {
    if selected {
        TAIL_LINES_FOCUSED
    } else {
        TAIL_LINES
    }
}

/// Build the right [`Block`] impl for a wire `BlockKind` + accumulated
/// text. Most kinds map onto a generic [`LabelledBlock`]; `Tailed`
/// (shell output, reasoning) has its own structural shape (header +
/// body tail) and gets a dedicated [`TailedBlock`].
pub fn block_for_kind(kind: WireBlockKind, text: String) -> Box<dyn Block> {
    match kind {
        WireBlockKind::Tailed { header } => Box::new(TailedBlock::new(header, text)),
        WireBlockKind::Diff { lines } => Box::new(DiffBlock::new(lines)),
        WireBlockKind::ToolUse {
            name,
            detail: Some(detail),
        } => Box::new(ToolUseBlock::new(name, detail)),
        other => Box::new(LabelledBlock::new(other, text)),
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiffBlock {
    lines: Vec<frances_session::events::DiffLine>,
}

impl DiffBlock {
    pub fn new(lines: Vec<frances_session::events::DiffLine>) -> Self {
        Self { lines }
    }
}

impl Input for DiffBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for DiffBlock {
    fn kind(&self) -> BlockKind {
        BlockKind::Diff
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        let max = ctx.width.max(1) as usize;
        let mut count = 0;
        for line in &self.lines {
            let content = match line {
                frances_session::events::DiffLine::Context { text: c, line: l } => {
                    format!("{:4} {}", l, c)
                }
                frances_session::events::DiffLine::Added(a) => a.to_string(),
                frances_session::events::DiffLine::Removed(r) => r.to_string(),
            };
            let mut out = Vec::new();
            wrap_into("", &content, max, &mut out);
            count += out.len().max(1) as u16;
        }
        count
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        let max = ctx.area.width.max(1) as usize;
        let mut row_writer = RowWriter::new(ctx);
        for line in &self.lines {
            let (content, style) = match line {
                frances_session::events::DiffLine::Context { text: c, line: l } => {
                    let formatted = format!("{:4} {}", l, c);
                    (formatted, Style::default())
                }
                frances_session::events::DiffLine::Added(a) => (
                    a.to_string(),
                    Style::default().bg(Color::Green).fg(Color::Black),
                ),
                frances_session::events::DiffLine::Removed(r) => (
                    r.to_string(),
                    Style::default().bg(Color::Red).fg(Color::Black),
                ),
            };

            let mut out = Vec::new();
            wrap_into("", &content, max, &mut out);

            for wrapped_line in out {
                let written = row_writer.write_styled(&wrapped_line, style);
                if !written && row_writer.finished() {
                    paint_truncation_marker_if_set(row_writer.ctx);
                    return;
                }
            }
        }
        paint_truncation_marker_if_set(row_writer.ctx);
    }
}

/// History row for a labelled (kind + text) block. Wraps to the
/// available width with the kind prefix on the first row and a
/// matching-width indent on continuation rows.
#[derive(Serialize, Deserialize)]
pub struct LabelledBlock {
    pub kind: WireBlockKind,
    pub text: String,
}

impl LabelledBlock {
    pub fn new(kind: WireBlockKind, text: String) -> Self {
        Self { kind, text }
    }
}

impl Input for LabelledBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for LabelledBlock {
    fn kind(&self) -> BlockKind {
        match self.kind {
            WireBlockKind::Text { .. } => BlockKind::Text,
            WireBlockKind::ToolUse { .. } => BlockKind::ToolUse,
            WireBlockKind::Tailed { .. } => BlockKind::Tailed,
            WireBlockKind::Diff { .. } => BlockKind::Diff,
        }
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        wrapped_block_lines(&self.kind, &self.text, ctx.width).len() as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        let lines = wrapped_block_lines(&self.kind, &self.text, ctx.area.width);
        let prefix = prefix_for(&self.kind);
        let prefix_bytes = prefix.len();
        let prefix_cols = display_width(&prefix) as u16;
        let prefix_style = prefix_style(&self.kind);
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, line) in lines.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            let y = area.y + dst_row;
            if i == 0 && line.starts_with(&prefix) {
                ctx.buf
                    .set_string(area.x, y, &line[..prefix_bytes], prefix_style);
                if line.len() > prefix_bytes {
                    ctx.buf.set_string(
                        area.x + prefix_cols,
                        y,
                        &line[prefix_bytes..],
                        Style::default(),
                    );
                }
            } else {
                ctx.buf.set_string(area.x, y, line, Style::default());
            }
        }
        paint_truncation_marker_if_set(ctx);
    }
}

/// History row for a tailed streaming-output block (shell command,
/// model reasoning). Renders as:
///   `[label] header_body` (the body may wrap; continuation rows are unindented)
///   `… [N earlier lines]` (only when the body is longer than the tail)
///   last-`TAIL_LINES` body lines (wrapped, unindented)
///
/// `header` drives both the prefix label/colour and any pinned text
/// after it (shell: the command line; reasoning: empty). It rides on
/// every `BlockDelta` so the header stays pinned even while the body
/// keeps streaming.
#[derive(Serialize, Deserialize)]
pub struct TailedBlock {
    pub header: TailedHeader,
    pub text: String,
    /// Alt-view-only scroll offset, measured in *source lines* from
    /// the tail. `0` = the window sits at the tail (the canonical
    /// live-view position). Live-view renders always force this to
    /// `0` before computing the visible window, so the persisted
    /// representation never carries scroll state — hence the
    /// `serde(default, skip)` annotation.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub scroll_y: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

impl TailedBlock {
    pub fn new(header: TailedHeader, text: String) -> Self {
        Self {
            header,
            text,
            scroll_y: 0,
        }
    }

    fn header_prefix(&self) -> (String, Style) {
        tailed_status(&self.header)
    }

    /// Text pinned after the status prefix on the first header line.
    /// Shell: the command line. Reasoning: empty.
    fn header_body(&self) -> &str {
        match &self.header {
            TailedHeader::Shell { cmd, .. } => cmd,
            TailedHeader::Reasoning { .. } => "",
        }
    }

    /// Left padding applied to body rows. Reasoning gets 2 cols so the
    /// thought text reads as a visually subordinate aside.
    fn body_indent(&self) -> u16 {
        match &self.header {
            TailedHeader::Reasoning { .. } => 2,
            TailedHeader::Shell { .. } => 0,
        }
    }

    /// Paint style for body rows. Reasoning is dimmed; shell output
    /// stays default so it remains scannable.
    fn body_style(&self) -> Style {
        match &self.header {
            TailedHeader::Reasoning { .. } => Style::default().add_modifier(Modifier::DIM),
            TailedHeader::Shell { .. } => Style::default(),
        }
    }

    fn header_lines(&self, width: u16) -> Vec<String> {
        let (prefix, _style) = self.header_prefix();
        let mut out = Vec::new();
        wrap_into(&prefix, self.header_body(), width.max(1) as usize, &mut out);
        out
    }

    /// Non-empty source lines, with trailing blanks from a closing
    /// `\n` stripped. Shared between [`Self::max_scroll_for`] and the
    /// windowing logic in [`Self::body_lines_at`].
    fn source_lines(&self) -> Vec<&str> {
        let mut source: Vec<&str> = self.text.split('\n').collect();
        while matches!(source.last(), Some(&"")) {
            source.pop();
        }
        source
    }

    /// Maximum legal `scroll_y` for a given tail height — beyond this
    /// the window would expose negative rows. Returns `0` when the
    /// source is shorter than the tail.
    fn max_scroll_for(&self, tail: usize) -> u16 {
        let source_len = self.source_lines().len();
        source_len.saturating_sub(tail) as u16
    }

    /// Body rows for a window of height `tail` whose right edge sits
    /// `window_start` source-lines *before* the natural tail. The row
    /// count is invariant in `window_start` (clamped via
    /// [`Self::max_scroll_for`]) so `measure` and `render` agree on height.
    ///
    /// - When `source.len() > tail`: 1 marker row + `tail` body rows.
    ///   The marker reports how many source lines remain hidden above
    ///   the window (`0` when scrolled to the top).
    /// - When `source.len() <= tail`: marker suppressed, output is the
    ///   source verbatim.
    fn body_lines_at(&self, width: u16, window_start: u16, tail: usize) -> Vec<String> {
        let source = self.source_lines();
        if source.is_empty() {
            return Vec::new();
        }
        let body_width = width.saturating_sub(self.body_indent());
        let max = body_width.max(1) as usize;
        let mut out = Vec::new();
        if source.len() > tail {
            let start_offset = window_start.min(self.max_scroll_for(tail)) as usize;
            let end = source.len() - start_offset;
            let start = end - tail;
            let marker = format!(
                "… [{start} earlier line{}]",
                if start == 1 { "" } else { "s" }
            );
            wrap_into("", &marker, max, &mut out);
            for line in &source[start..end] {
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

impl Input for TailedBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        event: &crossterm::event::Event,
    ) -> EventOutcome {
        let crossterm::event::Event::Key(key) = event else {
            return EventOutcome::Pass;
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return EventOutcome::Pass;
        }
        // Input events only reach this block when it's the alt-view
        // selection, so the focused tail is the right clamp.
        let max = self.max_scroll_for(TAIL_LINES_FOCUSED);
        let half: u16 = (TAIL_LINES_FOCUSED as u16) / 2;
        match key.code {
            crossterm::event::KeyCode::Char('j') => {
                self.scroll_y = self.scroll_y.saturating_sub(1);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('k') => {
                self.scroll_y = self.scroll_y.saturating_add(1).min(max);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('d') => {
                self.scroll_y = self.scroll_y.saturating_sub(half);
                EventOutcome::Consumed
            }
            crossterm::event::KeyCode::Char('u') => {
                self.scroll_y = self.scroll_y.saturating_add(half).min(max);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Block for TailedBlock {
    fn kind(&self) -> BlockKind {
        BlockKind::Tailed
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        let tail = tail_lines_for(ctx.selected);
        let body_rows = self.body_lines_at(ctx.width, 0, tail).len();
        (self.header_lines(ctx.width).len() + body_rows) as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        // Live view always shows the tail (window_start = 0).
        // Alt view honours the block's internal scroll position so the
        // user can page through earlier source lines. The tail grows
        // when this block is the alt-view selection.
        let window_start = if ctx.alt_view { self.scroll_y } else { 0 };
        let tail = tail_lines_for(ctx.selected);
        let header = self.header_lines(ctx.area.width);
        let body = self.body_lines_at(ctx.area.width, window_start, tail);
        let (prefix, prefix_style) = self.header_prefix();
        let prefix_bytes = prefix.len();
        let prefix_cols = display_width(&prefix) as u16;

        let src_y = ctx.src_y;
        let area = ctx.area;
        let mut src_idx: u16 = 0;
        for (i, line) in header.iter().enumerate() {
            if src_idx >= src_y {
                let dst_row = src_idx - src_y;
                if dst_row >= area.height {
                    paint_truncation_marker_if_set(ctx);
                    return;
                }
                let y = area.y + dst_row;
                if i == 0 && line.starts_with(&prefix) {
                    ctx.buf
                        .set_string(area.x, y, &line[..prefix_bytes], prefix_style);
                    if line.len() > prefix_bytes {
                        ctx.buf.set_string(
                            area.x + prefix_cols,
                            y,
                            &line[prefix_bytes..],
                            Style::default(),
                        );
                    }
                } else {
                    ctx.buf.set_string(area.x, y, line, Style::default());
                }
            }
            src_idx = src_idx.saturating_add(1);
        }
        let body_indent = self.body_indent();
        let body_style = self.body_style();
        for line in body.iter() {
            if src_idx >= src_y {
                let dst_row = src_idx - src_y;
                if dst_row >= area.height {
                    paint_truncation_marker_if_set(ctx);
                    return;
                }
                ctx.buf.set_string(
                    area.x.saturating_add(body_indent),
                    area.y + dst_row,
                    line,
                    body_style,
                );
            }
            src_idx = src_idx.saturating_add(1);
        }
        paint_truncation_marker_if_set(ctx);
    }
}

/// History row for a one-shot tool call with a detail suffix. Renders
/// as `→ {name}  {detail}` on one line — the prefix in yellow, the
/// detail in dim — wrapping `detail` to subsequent rows (still dim,
/// indented to match the prefix column) when the line overflows.
///
/// The plain `BlockKind::ToolUse` variant (no detail) still routes
/// through [`LabelledBlock`]; only the `Some(detail)` shape comes here.
#[derive(Serialize, Deserialize)]
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

impl Input for ToolUseBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for ToolUseBlock {
    /// One-shot block — emitted as `BlockDelta` + `BlockStop`
    /// back-to-back by the runtime. The container promotes it straight
    /// to `safe` so it never carries the in-flight spinner overlay.
    fn safe_on_push(&self) -> bool {
        true
    }

    fn kind(&self) -> BlockKind {
        BlockKind::ToolUse
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        self.wrapped_lines(ctx.width).len() as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        let lines = self.wrapped_lines(ctx.area.width);
        let prefix = self.name_prefix();
        // Split the prefix into the colored arrow+name segment (yellow)
        // and the trailing two spaces that bridge into the dim detail.
        // The arrow+name byte count is `prefix.len() - 2` because the
        // suffix is exactly `"  "` (two ASCII spaces).
        let arrow_bytes = prefix.len() - 2;
        let prefix_cols = display_width(&prefix) as u16;
        let arrow_style = Style::default().fg(Color::Yellow);
        let dim_style = Style::default().add_modifier(Modifier::DIM);
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, line) in lines.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            let y = area.y + dst_row;
            if i == 0 && line.starts_with(&prefix) {
                ctx.buf
                    .set_string(area.x, y, &line[..arrow_bytes], arrow_style);
                if line.len() > prefix.len() {
                    ctx.buf
                        .set_string(area.x + prefix_cols, y, &line[prefix.len()..], dim_style);
                }
            } else {
                ctx.buf.set_string(area.x, y, line, dim_style);
            }
        }
        paint_truncation_marker_if_set(ctx);
    }
}

fn tailed_status(header: &TailedHeader) -> (String, Style) {
    match header {
        TailedHeader::Shell { state, .. } => match state {
            ShellState::Running => status_prefix("…", StatusTone::Pending),
            ShellState::Success => status_prefix("ok", StatusTone::Success),
            ShellState::Exit(n) => status_prefix(&format!("exit {n}"), StatusTone::Failure),
        },
        TailedHeader::Reasoning { state } => match state {
            ReasoningState::Streaming => status_prefix("thinking…", StatusTone::Pending),
            ReasoningState::Done => status_prefix("thought", StatusTone::Settled),
        },
    }
}

/// History row that holds raw, pre-formatted lines and renders them
/// verbatim (no kind prefix, no re-wrap). Used for banner rows, usage
/// summaries, error / approval messages — anything not driven by the
/// runtime's [`WireBlockKind`] vocabulary that still wants to live in
/// the container's scrollback. `style` paints the whole block uniformly;
/// ANSI variants only by convention (RGB stays available for future
/// syntax-highlighted block types).
///
/// `RawBlock` does not derive serde — ratatui's `Style` doesn't ship
/// `Serialize` in our build, and `RawBlock` content is UI-side state
/// (banners, error overlays) that never round-trips through scrollback
/// persistence anyway.
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

impl Input for RawBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for RawBlock {
    /// One-shot block — banners and error overlays arrive fully
    /// formed; the container can promote them straight to `safe`.
    fn safe_on_push(&self) -> bool {
        true
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Raw
    }

    fn measure(&self, _ctx: &BlockMeasureContext<'_>) -> u16 {
        self.lines.len() as u16
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) {
        let src_y = ctx.src_y;
        let area = ctx.area;
        for (i, line) in self.lines.iter().enumerate() {
            let i = i as u16;
            if i < src_y {
                continue;
            }
            let dst_row = i - src_y;
            if dst_row >= area.height {
                break;
            }
            ctx.buf
                .set_string(area.x, area.y + dst_row, line, self.style);
        }
        paint_truncation_marker_if_set(ctx);
    }
}

/// Overlay the dim "⋯ truncated ⋯" indicator on the bottom row of
/// `ctx.area` when the container flagged this entry as truncated.
/// Replaces the trailing-row Convention that `TruncatedBlock` used to
/// own; per-block `render` decides whether to call this.
fn paint_truncation_marker_if_set(ctx: &mut BlockRenderContext<'_>) {
    if !ctx.truncated || ctx.area.height == 0 || ctx.area.width == 0 {
        return;
    }
    let y = ctx.area.y + ctx.area.height - 1;
    let marker = "  ⋯ truncated ⋯";
    ctx.buf
        .set_string(ctx.area.x, y, marker, Style::default().fg(Color::DarkGray));
}

/// Linear row writer used by the diff block — its iteration emits
/// already-wrapped lines without keeping an explicit row index. Walks
/// `ctx.src_y` for the skip phase, then `ctx.area.height` for the
/// emit phase, transparently for the caller.
struct RowWriter<'a, 'b> {
    ctx: &'a mut BlockRenderContext<'b>,
    src_idx: u16,
    finished: bool,
}

impl<'a, 'b> RowWriter<'a, 'b> {
    fn new(ctx: &'a mut BlockRenderContext<'b>) -> Self {
        Self {
            ctx,
            src_idx: 0,
            finished: false,
        }
    }

    /// Write `line` with `style`. Returns `true` if the line was
    /// emitted, `false` if it was skipped (above src_y) or dropped
    /// (below area.height).
    fn write_styled(&mut self, line: &str, style: Style) -> bool {
        let i = self.src_idx;
        self.src_idx = self.src_idx.saturating_add(1);
        if i < self.ctx.src_y {
            return false;
        }
        let dst_row = i - self.ctx.src_y;
        if dst_row >= self.ctx.area.height {
            self.finished = true;
            return false;
        }
        let area = self.ctx.area;
        self.ctx
            .buf
            .set_string(area.x, area.y + dst_row, line, style);
        let w = display_width(line) as u16;
        if w < area.width {
            self.ctx.buf.set_string(
                area.x + w,
                area.y + dst_row,
                " ".repeat((area.width - w) as usize),
                style,
            );
        }
        true
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

pub fn prefix_for(kind: &WireBlockKind) -> String {
    match kind {
        WireBlockKind::Text {
            source: Source::User,
        } => "> ".to_owned(),
        WireBlockKind::Text {
            source: Source::Assistant,
        } => "◆ ".to_owned(),
        WireBlockKind::Text {
            source: Source::Internal,
        } => String::new(),
        // The `detail`-bearing variant routes through `ToolUseBlock`; the
        // `LabelledBlock` path only sees plain tool-use markers.
        WireBlockKind::ToolUse { name, .. } => format!("→ {name}"),
        WireBlockKind::Tailed { .. } => {
            // Tailed blocks render through TailedBlock, which owns
            // their own prefix; LabelledBlock should never see this kind.
            String::new()
        }
        WireBlockKind::Diff { .. } => String::new(),
    }
}

fn prefix_style(kind: &WireBlockKind) -> Style {
    match kind {
        WireBlockKind::Text { .. } => Style::default(),
        WireBlockKind::ToolUse { .. } => Style::default().fg(Color::Yellow),
        WireBlockKind::Tailed { .. } => Style::default(),
        WireBlockKind::Diff { .. } => Style::default(),
    }
}

pub fn wrapped_block_lines(kind: &WireBlockKind, text: &str, width: u16) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use frances_session::events::DiffLine;

    fn assistant() -> WireBlockKind {
        WireBlockKind::Text {
            source: Source::Assistant,
        }
    }

    #[test]
    fn no_trailing_newline_is_unchanged() {
        let lines = wrapped_block_lines(&assistant(), "Hello", 80);
        assert_eq!(lines, vec!["◆ Hello"]);
    }

    #[test]
    fn single_trailing_newline_is_stripped() {
        let lines = wrapped_block_lines(&assistant(), "Hello\n", 80);
        assert_eq!(
            lines,
            vec!["◆ Hello"],
            "trailing `\\n` should not produce an indent-only continuation row"
        );
    }

    #[test]
    fn multiple_trailing_newlines_are_stripped() {
        let lines = wrapped_block_lines(&assistant(), "Hello\n\n\n", 80);
        assert_eq!(lines, vec!["◆ Hello"]);
    }

    #[test]
    fn mid_text_paragraph_break_is_preserved() {
        let lines = wrapped_block_lines(&assistant(), "One\n\nTwo", 80);
        assert_eq!(
            lines,
            vec!["◆ One".to_string(), "  ".to_string(), "  Two".to_string(),],
            "an internal `\\n\\n` is a real paragraph break and stays"
        );
    }

    #[test]
    fn mid_text_paragraph_break_with_trailing_newline_keeps_only_the_break() {
        let lines = wrapped_block_lines(&assistant(), "One\n\nTwo\n", 80);
        assert_eq!(
            lines,
            vec!["◆ One".to_string(), "  ".to_string(), "  Two".to_string(),]
        );
    }

    #[test]
    fn newline_only_text_collapses_to_just_the_prefix() {
        let lines = wrapped_block_lines(&assistant(), "\n", 80);
        assert_eq!(lines, vec!["◆ "]);
    }

    #[test]
    fn internal_text_block_with_trailing_newline_does_not_emit_blank_row() {
        let kind = WireBlockKind::Text {
            source: Source::Internal,
        };
        let lines = wrapped_block_lines(&kind, "Hello\n", 80);
        assert_eq!(lines, vec!["Hello"]);
    }

    /// One round-trip-serde test per serializable block variant.
    /// `RawBlock` is intentionally excluded — see its doc-comment.
    mod serde_roundtrip {
        use super::*;

        fn round_trip<T: serde::Serialize + for<'de> serde::Deserialize<'de>>(value: &T) -> T {
            let s = serde_json::to_string(value).expect("serialize");
            serde_json::from_str(&s).expect("deserialize")
        }

        #[test]
        fn diff_block() {
            let b = DiffBlock::new(vec![
                DiffLine::Context {
                    text: "ctx".into(),
                    line: 7,
                },
                DiffLine::Added("added".into()),
                DiffLine::Removed("removed".into()),
            ]);
            let r = round_trip(&b);
            assert_eq!(r.lines.len(), 3);
        }

        #[test]
        fn labelled_block_text() {
            let b = LabelledBlock::new(
                WireBlockKind::Text {
                    source: Source::Assistant,
                },
                "hello".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "hello");
            match r.kind {
                WireBlockKind::Text { source } => assert_eq!(source, Source::Assistant),
                _ => panic!("kind not preserved"),
            }
        }

        #[test]
        fn tailed_shell_block() {
            let b = TailedBlock::new(
                TailedHeader::Shell {
                    state: ShellState::Success,
                    cmd: "ls".into(),
                },
                "out".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "out");
            match r.header {
                TailedHeader::Shell { state, cmd } => {
                    assert_eq!(&*cmd, "ls");
                    assert!(matches!(state, ShellState::Success));
                }
                other => panic!("unexpected header: {other:?}"),
            }
        }

        #[test]
        fn tailed_reasoning_block() {
            let b = TailedBlock::new(
                TailedHeader::Reasoning {
                    state: ReasoningState::Streaming,
                },
                "thinking…".into(),
            );
            let r = round_trip(&b);
            assert_eq!(r.text, "thinking…");
            assert!(matches!(
                r.header,
                TailedHeader::Reasoning {
                    state: ReasoningState::Streaming,
                }
            ));
        }

        #[test]
        fn tool_use_block() {
            let b = ToolUseBlock::new("shell".into(), "ls -la".into());
            let r = round_trip(&b);
            assert_eq!(&*r.name, "shell");
            assert_eq!(&*r.detail, "ls -la");
        }
    }

    /// Phase D — `TailedBlock` scroll state + alt-view rendering.
    mod shell_scroll {
        use super::*;
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        use frances_tui::widget::{EventContext, EventOutcome, Focus, Theme};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        fn block_with_lines(n: usize) -> TailedBlock {
            let body: String = (0..n).map(|i| format!("line{}\n", i + 1)).collect();
            TailedBlock::new(
                TailedHeader::Shell {
                    state: ShellState::Success,
                    cmd: "cmd".into(),
                },
                body,
            )
        }

        fn press(c: char) -> Event {
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            })
        }

        fn dispatch(block: &mut TailedBlock, event: &Event) -> EventOutcome {
            let mut focus = Focus::new();
            let mut redraw = false;
            let mut ctx = EventContext {
                focus: &mut focus,
                redraw: &mut redraw,
            };
            block.handle_event(&mut ctx, event)
        }

        #[test]
        fn j_decrements_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 5;
            assert!(matches!(
                dispatch(&mut b, &press('j')),
                EventOutcome::Consumed
            ));
            assert_eq!(b.scroll_y, 4);
        }

        #[test]
        fn j_at_zero_is_noop() {
            let mut b = block_with_lines(30);
            assert_eq!(b.scroll_y, 0);
            dispatch(&mut b, &press('j'));
            assert_eq!(b.scroll_y, 0);
        }

        #[test]
        fn k_clamps_at_source_minus_focused_tail() {
            // 25 lines, focused tail = 20 → max_scroll = 5.
            let mut b = block_with_lines(25);
            for _ in 0..20 {
                dispatch(&mut b, &press('k'));
            }
            assert_eq!(b.scroll_y, 5);
        }

        #[test]
        fn d_u_step_by_half_focused_tail() {
            // 40 lines, focused tail = 20 → max_scroll = 20, half = 10.
            let mut b = block_with_lines(40);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 10);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 20);
            dispatch(&mut b, &press('u'));
            assert_eq!(b.scroll_y, 20, "u saturates at max_scroll");
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 10);
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 0);
            dispatch(&mut b, &press('d'));
            assert_eq!(b.scroll_y, 0, "d saturates at zero");
        }

        #[test]
        fn body_lines_shift_with_scroll_y() {
            let b0 = block_with_lines(30);
            let mut b5 = block_with_lines(30);
            b5.scroll_y = 5;
            let lines0 = b0.body_lines_at(80, 0, TAIL_LINES);
            let lines5 = b5.body_lines_at(80, 5, TAIL_LINES);
            // Both runs have 1 marker + TAIL_LINES body rows.
            assert_eq!(lines0.len(), 1 + TAIL_LINES);
            assert_eq!(lines5.len(), 1 + TAIL_LINES);
            assert_eq!(lines0[0], "… [20 earlier lines]");
            assert_eq!(lines5[0], "… [15 earlier lines]");
            // Window shifts back by 5 source lines.
            assert_eq!(lines0[1], "line21");
            assert_eq!(lines5[1], "line16");
        }

        #[test]
        fn live_view_always_shows_tail_regardless_of_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 7;
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
            let theme = Theme::default();
            let mut ctx = BlockRenderContext {
                area: Rect::new(0, 0, 40, 12),
                buf: &mut buf,
                src_y: 0,
                truncated: false,
                alt_view: false,
                selected: false,
                theme: &theme,
            };
            b.render(&mut ctx);
            // First body row = first row after the header (1 row).
            // Live view forces window_start = 0 → marker "20 earlier lines".
            let marker_row: String = (0..40)
                .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(
                marker_row.starts_with("… [20 earlier lines]"),
                "live view ignored scroll_y; got marker row: {marker_row:?}",
            );
        }

        #[test]
        fn alt_view_honours_scroll_y() {
            let mut b = block_with_lines(30);
            b.scroll_y = 7;
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
            let theme = Theme::default();
            let mut ctx = BlockRenderContext {
                area: Rect::new(0, 0, 40, 12),
                buf: &mut buf,
                src_y: 0,
                truncated: false,
                alt_view: true,
                selected: false,
                theme: &theme,
            };
            b.render(&mut ctx);
            let marker_row: String = (0..40)
                .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
                .collect();
            assert!(
                marker_row.starts_with("… [13 earlier lines]"),
                "alt view ignored scroll_y; got marker row: {marker_row:?}",
            );
        }
    }
}
